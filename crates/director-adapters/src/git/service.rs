//! [`GitService`] — the observation service that turns git facts into project state.
//!
//! This is where the three responsibilities of Phase 2 meet, and it is the only
//! place they are allowed to meet:
//!
//! 1. [`GitObserver`] answers *"what happened in git?"* — raw facts, nothing more.
//! 2. The domain types answer *"what does this mean for the project state?"*
//!    through pure functions like [`ProjectStateSnapshot::state_changes`].
//! 3. This service records the answer persistently and bumps the version.
//!
//! It does **not** answer *"what should happen next?"* — no task is looked at,
//! no agent is considered, no plan is touched. That separation is the
//! architectural rule for the whole phase, and it is why a caller cannot get
//! planning behaviour out of this service no matter what it passes in.
//!
//! ## The sync contract
//!
//! [`GitService::sync_repository`] is idempotent and incremental by
//! construction: it walks only the commit range it has not seen, derives events
//! only for facts it has not recorded, and bumps the state version only when an
//! important change actually occurred.

use director_domain::ids::{ProjectId, RepositoryId};
use director_domain::repository::{
    EventKind, FileChangeRecord, ObservationEvent, ProjectStateSnapshot, Repository,
    RepositoryError, RepositoryStatus, StateChangeKind, SyncResult, SyncStatus, WorktreeState,
};
use director_domain::state::CommitInfo;

use crate::git::observer::GitObserver;
use crate::git::store::RepositoryStore;

/// How many recent commits an initial observation records as its baseline.
const BASELINE_COMMIT_LIMIT: usize = 50;

/// The observed state of one repository, in one answer.
#[derive(Debug, Clone)]
pub struct RepositoryStatusView {
    /// The registered repository record.
    pub repository: Repository,
    /// The latest snapshot.
    pub snapshot: ProjectStateSnapshot,
}

/// Read, compare, and persist the state of git working trees.
///
/// Cheaply cloneable: the store is shared behind an [`std::sync::Arc`], so two
/// agent sessions can each hold a service and still serialize on one lock.
#[derive(Debug, Clone)]
pub struct GitService {
    observer: GitObserver,
    store: std::sync::Arc<RepositoryStore>,
}

impl GitService {
    /// Create a service that persists under `state_dir`.
    pub fn new(state_dir: impl AsRef<std::path::Path>) -> Result<Self, RepositoryError> {
        Ok(GitService {
            observer: GitObserver::new(),
            store: std::sync::Arc::new(RepositoryStore::new(state_dir)?),
        })
    }

    /// The directory this service persists to. Exposed for diagnostics.
    pub fn state_dir(&self) -> &std::path::Path {
        self.store.root()
    }

    /// Initialize observation of a git repository (spec §15).
    ///
    /// Validates the path, verifies it is a git repository, reads its current
    /// state, stores the metadata, and establishes the observation baseline.
    /// A directory that is not a git repository is never silently accepted.
    pub async fn initialize_repository(
        &self,
        project_id: ProjectId,
        repository_id: RepositoryId,
        local_path: &str,
    ) -> Result<Repository, RepositoryError> {
        let repo = self.observer.open(local_path)?;
        let branch = self.observer.read_branch(&repo);
        let head = self.observer.read_head_commit(&repo).unwrap_or_default();
        let status = self.observer.read_status(&repo)?;
        let remote_url = self.observer.read_remote_url(&repo);
        let default_branch = self.observer.read_default_branch(&repo);
        let worktree = self.observer.read_worktree(&repo, &repository_id)?;
        let now = chrono::Utc::now();

        let last_observed_commit = if head.is_empty() {
            None
        } else {
            Some(head.clone())
        };
        let repository = Repository::new(
            repository_id.clone(),
            project_id.clone(),
            local_path,
            remote_url,
            default_branch,
            branch.clone(),
            head.clone(),
            status,
            last_observed_commit,
        );

        let snapshot = ProjectStateSnapshot::initial(
            project_id.clone(),
            repository_id.clone(),
            branch,
            head,
            worktree,
            now,
        );

        // The baseline commit window is recorded as *observed*, not as newly
        // created: Director did not watch these commits happen, it arrived and
        // found them. Only commits after this point become COMMIT_CREATED.
        let baseline_commits = if status == RepositoryStatus::Empty {
            vec![]
        } else {
            self.observer
                .read_recent_commits(&repo, BASELINE_COMMIT_LIMIT)?
        };

        let baseline_event = ObservationEvent::sync(&snapshot, now);
        let mut record =
            self.store
                .initialize(repository.clone(), snapshot, vec![baseline_event])?;
        record.append_commits(baseline_commits);

        // Persist the baseline commits without producing events.
        self.store.with_locked(&repository_id, |stored| {
            stored.commits = record.commits.clone();
            Ok(())
        })?;

        Ok(repository)
    }

    /// Synchronize one repository against git (spec §16).
    ///
    /// Reads current state, compares with the previous state, detects changes,
    /// persists the new state, emits normalized events, updates the project
    /// state version, and returns a structured result.
    pub async fn sync_repository(
        &self,
        repository_id: &RepositoryId,
    ) -> Result<SyncResult, RepositoryError> {
        self.store.with_locked(repository_id, |stored| {
            let now = chrono::Utc::now();
            let project_id = stored.repository.project_id.clone();
            let local_path = stored.repository.local_path.clone();
            let repo = self.observer.open(&local_path)?;
            let branch = self.observer.read_branch(&repo);
            let head = self.observer.read_head_commit(&repo).unwrap_or_default();
            let status = self.observer.read_status(&repo)?;
            let worktree = self.observer.read_worktree(&repo, repository_id)?;

            let previous = stored.snapshot.clone();

            // 2. Build the candidate snapshot, anchored on the previous
            //    observation's last processed commit.
            let mut current = ProjectStateSnapshot::initial(
                project_id.clone(),
                repository_id.clone(),
                branch.clone(),
                head.clone(),
                worktree,
                now,
            );
            current.last_observed_commit = previous.last_observed_commit.clone();
            current.state_version = previous.state_version;

            // 3. Detect changes, deterministically.
            let changes = current.state_changes(&previous);

            // 4. Incremental commit walk over the range not yet processed.
            let walk = self.observer.walk_new_commits(
                &repo,
                Some(&previous.last_observed_commit),
                &head,
            )?;

            let mut events: Vec<ObservationEvent> = Vec::new();
            let mut new_commits: Vec<CommitInfo> = Vec::new();
            let mut new_file_changes: Vec<FileChangeRecord> = Vec::new();

            for commit in &walk.commits {
                events.push(ObservationEvent::commit(
                    repository_id,
                    &project_id,
                    commit.clone(),
                    now,
                ));
                let changes =
                    self.observer
                        .read_commit_changes(&repo, repository_id, &commit.sha)?;
                for change in changes {
                    if let Some(event) = ObservationEvent::file(change.clone(), &project_id, now) {
                        events.push(event);
                    }
                    new_file_changes.push(change);
                }
                new_commits.push(commit.clone());
            }

            // 5. Worktree changes: only the ones not already recorded.
            for change in current.working_tree.new_changes(&previous.working_tree) {
                if let Some(event) = ObservationEvent::file(change, &project_id, now) {
                    events.push(event);
                }
            }

            // 6. Branch change.
            if changes.contains(&StateChangeKind::BranchChanged) {
                events.push(ObservationEvent::branch_change(
                    repository_id,
                    &project_id,
                    previous.branch.clone(),
                    branch.clone(),
                    &head,
                    now,
                ));
            }

            // 7. The sync itself, recorded once per distinct observed state.
            events.push(ObservationEvent::sync(&current, now));

            // 8. Persist: events are deduplicated by their stable ids, so a
            //    no-op re-sync records nothing new.
            stored.append_commits(new_commits);
            stored.append_file_changes(new_file_changes);
            let events_created = stored.append_events(events);

            // 9. Advance the version exactly once when an important change
            //    occurred. This is Phase 2's only versioning mechanism: there
            //    is no second counter anywhere.
            if current.has_important_change(&previous) {
                current.state_version = previous.state_version + 1;
            }
            current.last_observed_commit = head.clone();
            stored.set_snapshot(current.clone());

            stored
                .repository
                .mark_observed(branch, head.clone(), status);
            if head.is_empty() {
                stored.repository.last_observed_commit = None;
            } else {
                stored.repository.set_last_observed_commit(head.clone());
            }

            let sync_status = if previous.last_observed_commit.is_empty()
                && previous.state_version == 1
                && walk.commits.is_empty()
                && events_created <= 1
            {
                // First real observation after initialization recorded only
                // its own sync event — the baseline, not a change.
                SyncStatus::Baseline
            } else if events_created == 0 {
                SyncStatus::NoChange
            } else {
                SyncStatus::Synced
            };

            Ok(SyncResult {
                status: sync_status,
                branch_changed: changes.contains(&StateChangeKind::BranchChanged),
                commit_changed: changes.contains(&StateChangeKind::CommitChanged),
                working_tree_changed: changes.contains(&StateChangeKind::WorktreeChanged),
                events_created,
                previous_commit: if previous.last_observed_commit.is_empty() {
                    None
                } else {
                    Some(previous.last_observed_commit.clone())
                },
                current_commit: head,
                state_version: current.state_version,
            })
        })
    }

    /// The registered repository record.
    pub fn repository(&self, repository_id: &RepositoryId) -> Result<Repository, RepositoryError> {
        Ok(self.store.load(repository_id)?.repository)
    }

    /// The observed status of a repository: its record and latest snapshot.
    pub fn status(
        &self,
        repository_id: &RepositoryId,
    ) -> Result<RepositoryStatusView, RepositoryError> {
        let stored = self.store.load(repository_id)?;
        Ok(RepositoryStatusView {
            repository: stored.repository,
            snapshot: stored.snapshot,
        })
    }

    /// The latest snapshot for a repository.
    pub fn snapshot(
        &self,
        repository_id: &RepositoryId,
    ) -> Result<ProjectStateSnapshot, RepositoryError> {
        Ok(self.store.load(repository_id)?.snapshot)
    }

    /// The latest snapshot for a project, resolving the project to its
    /// repository. Returns `NotRegistered` if the project has no repository.
    pub fn snapshot_for_project(
        &self,
        project_id: &ProjectId,
    ) -> Result<ProjectStateSnapshot, RepositoryError> {
        for id in self.store.list()? {
            let stored = self.store.load(&id)?;
            if stored.repository.project_id == *project_id {
                return Ok(stored.snapshot);
            }
        }
        Err(RepositoryError::NotRegistered(project_id.to_string()))
    }

    /// Every repository registered under a project.
    pub fn repositories_for_project(
        &self,
        project_id: &ProjectId,
    ) -> Result<Vec<Repository>, RepositoryError> {
        let mut found = Vec::new();
        for id in self.store.list()? {
            let stored = self.store.load(&id)?;
            if stored.repository.project_id == *project_id {
                found.push(stored.repository);
            }
        }
        found.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        Ok(found)
    }

    /// Every registered repository id.
    pub fn list_repositories(&self) -> Result<Vec<RepositoryId>, RepositoryError> {
        self.store.list()
    }

    /// The file changes recorded for a repository, newest observation first.
    pub fn changes(
        &self,
        repository_id: &RepositoryId,
    ) -> Result<Vec<FileChangeRecord>, RepositoryError> {
        let mut changes = self.store.load(repository_id)?.file_changes;
        changes.sort_by_key(|change| std::cmp::Reverse(change.observed_at));
        Ok(changes)
    }

    /// The commits recorded for a repository, newest first.
    pub fn commits(
        &self,
        repository_id: &RepositoryId,
    ) -> Result<Vec<CommitInfo>, RepositoryError> {
        Ok(self.store.load(repository_id)?.commits)
    }

    /// The events recorded for a repository, newest first.
    pub fn events(
        &self,
        repository_id: &RepositoryId,
    ) -> Result<Vec<ObservationEvent>, RepositoryError> {
        let mut events = self.store.load(repository_id)?.events;
        events.sort_by_key(|event| std::cmp::Reverse(event.timestamp));
        Ok(events)
    }

    /// Normalized diff information for the working tree right now, with line
    /// counts. Computed on demand — this is the only operation that
    /// materializes patches, and it is not cached.
    pub fn diff(
        &self,
        repository_id: &RepositoryId,
    ) -> Result<Vec<director_domain::repository::DiffInfo>, RepositoryError> {
        let stored = self.store.load(repository_id)?;
        let repo = self.observer.open(&stored.repository.local_path)?;
        self.observer
            .read_diff(&repo, stored.snapshot.branch.as_deref())
    }

    /// The kinds of events recorded for a repository, with counts. A diagnostic
    /// convenience for the CLI and for tests asserting what was observed.
    pub fn event_counts(
        &self,
        repository_id: &RepositoryId,
    ) -> Result<Vec<(EventKind, usize)>, RepositoryError> {
        let events = self.events(repository_id)?;
        let mut counts: Vec<(EventKind, usize)> = Vec::new();
        for event in events {
            if let Some(entry) = counts.iter_mut().find(|(kind, _)| *kind == event.kind) {
                entry.1 += 1;
            } else {
                counts.push((event.kind, 1));
            }
        }
        counts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        Ok(counts)
    }

    /// Read the live worktree state without persisting anything. For callers
    /// that need to ask "what is the tree like right now" without a sync.
    pub fn observe_worktree(
        &self,
        repository_id: &RepositoryId,
    ) -> Result<WorktreeState, RepositoryError> {
        let stored = self.store.load(repository_id)?;
        let repo = self.observer.open(&stored.repository.local_path)?;
        self.observer.read_worktree(&repo, repository_id)
    }
}

#[cfg(test)]
mod service_logic_tests {
    //! Pure-logic tests over the service's decision rules. The git-backed
    //! behaviour is exercised in `tests/git_integration.rs` against real
    //! repositories; these tests cover the pieces that do not need git at all.

    use super::*;
    use director_domain::repository::WorktreeState;

    #[test]
    fn a_status_view_carries_the_snapshot_and_record() {
        // Construction-only smoke test: the real paths are covered by the
        // integration suite, which builds genuine git repositories.
        let snapshot = ProjectStateSnapshot::initial(
            ProjectId::from_string("PROJ-1"),
            RepositoryId::from_string("REPO-1"),
            Some("main".into()),
            "abc".into(),
            WorktreeState::default(),
            chrono::Utc::now(),
        );
        assert_eq!(snapshot.state_version, 1);
    }
}
