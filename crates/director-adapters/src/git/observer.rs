//! [`GitObserver`] — the git2-backed implementation of "what happened in git?".
//!
//! ## What this is, and what it is not
//!
//! The observer is a **read-only lens onto one working tree**. It opens a
//! repository, reads facts, and hands back domain types. It persists nothing,
//! compares nothing against history, and emits no events — those are
//! [`crate::git::service::GitService`] and [`crate::git::store::RepositoryStore`].
//!
//! The separation is the architectural rule of Phase 2:
//!
//! > Git observer: "what happened in git?"
//! > Project state: "what does this mean for the project?"
//! > Director: "what should happen next?"
//!
//! Nothing here answers the third question, and very little here answers the
//! second.
//!
//! ## Security
//!
//! The observer exposes a fixed set of named operations. It accepts a
//! **path**, never a command: there is no method on this type through which a
//! caller could get arbitrary text executed, because it never builds a command
//! at all. git2 is an in-process library, so there is no shell in the loop to
//! inject into.
//!
//! Paths are validated before opening ([`Repository::validate_path`]) and the
//! opened repository's working directory is checked back against the requested
//! path, so a caller cannot point Director at a subdirectory and have it
//! silently observe a repository rooted elsewhere.

use std::path::Path;

use git2::{
    Commit, Delta, Diff, DiffOptions, Oid, Repository as Git2Repository, Signature, Status,
};

use director_domain::ids::RepositoryId;
use director_domain::repository::{
    FileChangeRecord, RepositoryError, RepositoryStatus, WorktreeState, SHORT_SHA_LEN,
};
use director_domain::state::{CommitInfo, FileChange};

/// Fingerprint used for a deletion, which has no content to hash.
const DELETED_FINGERPRINT: &str = "deleted";

/// Fingerprint used when git hands back a zero object id but a file is present.
/// Distinct from any real blob oid, which is 40 hex characters.
const UNKNOWN_FINGERPRINT: &str = "unknown";

/// The maximum number of commits one incremental walk will process. Bounds the
/// work after a force-push or a first observation of a long history; the
/// alternative is an unbounded walk on a repository with a million commits.
const MAX_WALK: usize = 200;

/// The result of walking the commit range between two observations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeWalk {
    /// New commits, oldest first. This is the order they happened in, which is
    /// the order events should be emitted in.
    pub commits: Vec<CommitInfo>,
    /// True when the commit the previous observation anchored on cannot be
    /// found in current history — a rebase, an amend, or a force-push moved the
    /// ground beneath the last snapshot. The walk falls back to a bounded
    /// recent window rather than failing.
    pub history_rewritten: bool,
}

/// Reads facts out of a git working tree.
#[derive(Debug, Clone, Default)]
pub struct GitObserver;

impl GitObserver {
    /// Create an observer. Stateless: every call opens the repository afresh,
    /// which is what makes observations independent of each other.
    pub fn new() -> Self {
        GitObserver
    }

    /// Validate a path, canonicalize it, and open it as a git repository.
    ///
    /// Returns [`RepositoryError::NotARepository`] for a directory that is not
    /// one, and [`RepositoryError::UnsafePath`] for a path that fails
    /// validation or that resolves to a repository rooted somewhere else.
    pub fn open(&self, path: &str) -> Result<Git2Repository, RepositoryError> {
        director_domain::repository::Repository::validate_path(path)?;

        let canonical = std::fs::canonicalize(path).map_err(|_| RepositoryError::PathMissing {
            path: path.to_string(),
        })?;
        let canonical_str = canonical
            .to_str()
            .ok_or_else(|| RepositoryError::UnsafePath {
                reason: "repository path is not valid UTF-8".into(),
            })?;
        director_domain::repository::Repository::validate_path(canonical_str)?;

        let repo =
            Git2Repository::open(canonical_str).map_err(|_| RepositoryError::NotARepository {
                path: canonical_str.to_string(),
            })?;

        // `Repository::open` searches upward. If it found a repository rooted
        // above the requested path, refuse it: Director must observe exactly
        // the working tree it was pointed at, never a parent it did not
        // authorize.
        let workdir = repo
            .workdir()
            .ok_or_else(|| RepositoryError::NotARepository {
                path: canonical_str.to_string(),
            })?;
        let workdir_canonical =
            std::fs::canonicalize(workdir).map_err(|_| RepositoryError::UnsafePath {
                reason: "repository working directory is not readable".into(),
            })?;
        if workdir_canonical != canonical {
            return Err(RepositoryError::UnsafePath {
                reason: format!(
                    "path {} is inside a repository rooted at {}, which is outside the authorized scope",
                    canonical.display(),
                    workdir_canonical.display()
                ),
            });
        }

        Ok(repo)
    }

    /// The branch `HEAD` points at, or `None` under a detached HEAD.
    pub fn read_branch(&self, repo: &Git2Repository) -> Option<String> {
        let head = repo.head().ok()?;
        if head.is_branch() {
            head.shorthand().map(str::to_string).ok()
        } else {
            None
        }
    }

    /// The commit `HEAD` points at.
    ///
    /// Errors with [`RepositoryError::Empty`] when the repository has no commits
    /// — an unborn branch is a real state, not a missing value, and callers
    /// must not invent a SHA for it.
    pub fn read_head_commit(&self, repo: &Git2Repository) -> Result<String, RepositoryError> {
        match repo.head() {
            Ok(reference) => reference
                .target()
                .map(|oid| oid.to_string())
                .ok_or(RepositoryError::Empty),
            Err(_) => Err(RepositoryError::Empty),
        }
    }

    /// The coarse status of the repository itself.
    pub fn read_status(&self, repo: &Git2Repository) -> Result<RepositoryStatus, RepositoryError> {
        match repo.head() {
            Ok(reference) => {
                if reference.is_branch() {
                    Ok(RepositoryStatus::OnBranch)
                } else {
                    Ok(RepositoryStatus::DetachedHead)
                }
            }
            // An unborn branch means no commits yet.
            Err(_) => Ok(RepositoryStatus::Empty),
        }
    }

    /// The URL of the `origin` remote when there is one. Many repositories have
    /// no remote; `None` is normal.
    pub fn read_remote_url(&self, repo: &Git2Repository) -> Option<String> {
        repo.find_remote("origin")
            .ok()
            .and_then(|remote| remote.url().map(str::to_string).ok())
    }

    /// The branch the repository treats as its trunk.
    ///
    /// Determined from local branches, not assumed: `main` is tried first, then
    /// `master`, then whichever local branch sorts first, then `None`.
    pub fn read_default_branch(&self, repo: &Git2Repository) -> Option<String> {
        for candidate in ["main", "master"] {
            if repo.find_branch(candidate, git2::BranchType::Local).is_ok() {
                return Some(candidate.to_string());
            }
        }
        repo.branches(Some(git2::BranchType::Local))
            .ok()
            .and_then(|mut branches| {
                branches
                    .next()
                    .and_then(|branch| branch.ok())
                    .and_then(|(branch, _)| branch.name().ok().flatten().map(str::to_string))
            })
    }

    /// The working tree as staged and unstaged changes against `HEAD`.
    ///
    /// Both dimensions are reported because git reports both, and dropping one
    /// would lose information: a file that is staged as new *and* modified in
    /// the working tree is two facts, and both are kept. A path may therefore
    /// legitimately appear in more than one bucket.
    ///
    /// Line counts are not computed here — a status read should stay cheap on a
    /// large tree. Use [`Self::read_diff`] when line counts are needed.
    pub fn read_worktree(
        &self,
        repo: &Git2Repository,
        repository_id: &RepositoryId,
    ) -> Result<WorktreeState, RepositoryError> {
        let anchor = self.read_head_commit(repo).unwrap_or_default();
        let now = chrono::Utc::now();

        let staged = self.staged_changes(repo, repository_id, &anchor, now)?;
        let unstaged = self.unstaged_changes(repo, repository_id, &anchor, now)?;

        let mut records = staged;
        records.extend(unstaged);
        Ok(WorktreeState::from_changes(records))
    }

    /// Changes staged in the index, measured against `HEAD`.
    fn staged_changes(
        &self,
        repo: &Git2Repository,
        repository_id: &RepositoryId,
        anchor: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<FileChangeRecord>, RepositoryError> {
        let head_tree = repo.head().ok().and_then(|r| r.peel_to_tree().ok());
        let mut options = DiffOptions::new();

        let mut diff = repo
            .diff_tree_to_index(head_tree.as_ref(), None, Some(&mut options))
            .map_err(|err| RepositoryError::ObservationFailed(format!("staged diff: {err}")))?;
        detect_renames(&mut diff)?;

        self.records_from_diff(repo, &diff, repository_id, anchor, now)
    }

    /// Unstaged changes in the working tree, measured against the index,
    /// including untracked files.
    fn unstaged_changes(
        &self,
        repo: &Git2Repository,
        repository_id: &RepositoryId,
        anchor: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<FileChangeRecord>, RepositoryError> {
        let index = repo
            .index()
            .map_err(|err| RepositoryError::ObservationFailed(format!("read index: {err}")))?;
        let mut options = DiffOptions::new();
        options.include_untracked(true).include_unmodified(false);

        let mut diff = repo
            .diff_index_to_workdir(Some(&index), Some(&mut options))
            .map_err(|err| RepositoryError::ObservationFailed(format!("unstaged diff: {err}")))?;
        detect_renames(&mut diff)?;

        self.records_from_diff(repo, &diff, repository_id, anchor, now)
    }

    /// Turn a diff into change records.
    fn records_from_diff(
        &self,
        repo: &Git2Repository,
        diff: &Diff,
        repository_id: &RepositoryId,
        anchor: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<FileChangeRecord>, RepositoryError> {
        let mut records = Vec::new();
        for delta in diff.deltas() {
            let change_type = map_delta_type(delta.status());
            let new_path = delta
                .new_file()
                .path()
                .map(normalize_path)
                .unwrap_or_default();
            // For a deletion the interesting path is the one that went away.
            let path = if change_type == FileChange::Deleted {
                delta
                    .old_file()
                    .path()
                    .map(normalize_path)
                    .unwrap_or(new_path.clone())
            } else {
                new_path
            };
            if path.is_empty() {
                continue;
            }
            let old_path = if change_type.has_old_path() {
                delta
                    .old_file()
                    .path()
                    .map(normalize_path)
                    .filter(|old| old != &path)
            } else {
                None
            };
            let fingerprint =
                self.fingerprint_for(repo, delta.new_file().id(), delta.status(), &path);

            let record = FileChangeRecord::new(
                repository_id.clone(),
                path,
                old_path,
                change_type,
                fingerprint,
                anchor,
                now,
            )?;
            records.push(record);
        }
        Ok(records)
    }

    /// A content fingerprint for one side of a delta.
    fn fingerprint_for(
        &self,
        repo: &Git2Repository,
        oid: Oid,
        status: Delta,
        path: &str,
    ) -> String {
        if status == Delta::Deleted {
            return DELETED_FINGERPRINT.to_string();
        }
        if !oid.is_zero() {
            return oid.to_string();
        }
        // git sometimes hands back a zero oid for a workdir file it did not
        // hash (notably some untracked paths). Fall back to filesystem
        // metadata, which is stable enough to detect "same content still
        // there" between two observations.
        match repo.workdir() {
            Some(root) => match std::fs::metadata(root.join(path)) {
                Ok(meta) => {
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
                    format!(
                        "len:{}:mtime:{}",
                        meta.len(),
                        mtime.map(|d| d.as_secs()).unwrap_or(0)
                    )
                }
                Err(_) => UNKNOWN_FINGERPRINT.to_string(),
            },
            None => UNKNOWN_FINGERPRINT.to_string(),
        }
    }

    /// Read one commit by SHA, or fail.
    pub fn read_commit(
        &self,
        repo: &Git2Repository,
        sha: &str,
    ) -> Result<CommitInfo, RepositoryError> {
        let oid = parse_oid(repo, sha)?;
        let commit = repo
            .find_commit(oid)
            .map_err(|_| RepositoryError::ObservationFailed(format!("unknown commit {sha}")))?;
        Ok(commit_info(&commit))
    }

    /// The most recent commits, newest first, bounded by `limit`.
    pub fn read_recent_commits(
        &self,
        repo: &Git2Repository,
        limit: usize,
    ) -> Result<Vec<CommitInfo>, RepositoryError> {
        let mut walk = repo
            .revwalk()
            .map_err(|err| RepositoryError::ObservationFailed(format!("revwalk: {err}")))?;
        walk.push_head()
            .map_err(|err| RepositoryError::ObservationFailed(format!("push head: {err}")))?;

        let mut commits = Vec::new();
        for oid in walk.take(limit) {
            let oid =
                oid.map_err(|err| RepositoryError::ObservationFailed(format!("walk: {err}")))?;
            let commit = repo
                .find_commit(oid)
                .map_err(|err| RepositoryError::ObservationFailed(format!("commit: {err}")))?;
            commits.push(commit_info(&commit));
        }
        Ok(commits)
    }

    /// Walk the commits between a previous observation's anchor and the current
    /// `HEAD`, oldest first.
    ///
    /// - Previous equal to `HEAD`: nothing new.
    /// - Previous is an ancestor of `HEAD`: exactly the range between them.
    /// - Previous cannot be found (rebase, amend, force-push) or is unknown:
    ///   a bounded recent window, flagged with [`RangeWalk::history_rewritten`].
    ///
    /// This is Phase 2's incremental observation: the observer never walks a
    /// whole history when a range will do.
    pub fn walk_new_commits(
        &self,
        repo: &Git2Repository,
        previous: Option<&str>,
        head: &str,
    ) -> Result<RangeWalk, RepositoryError> {
        if let Some(previous) = previous {
            if !previous.is_empty() && previous == head {
                return Ok(RangeWalk {
                    commits: vec![],
                    history_rewritten: false,
                });
            }
        }

        let mut walk = repo
            .revwalk()
            .map_err(|err| RepositoryError::ObservationFailed(format!("revwalk: {err}")))?;
        walk.push_head()
            .map_err(|err| RepositoryError::ObservationFailed(format!("push head: {err}")))?;

        let previous_oid = previous
            .filter(|sha| !sha.is_empty())
            .and_then(|sha| parse_oid(repo, sha).ok());

        let mut found = Vec::new();
        let mut reached_anchor = false;
        let mut history_rewritten = false;

        for oid in walk.take(MAX_WALK) {
            let oid =
                oid.map_err(|err| RepositoryError::ObservationFailed(format!("walk: {err}")))?;
            if previous_oid == Some(oid) {
                reached_anchor = true;
                break;
            }
            let commit = repo
                .find_commit(oid)
                .map_err(|err| RepositoryError::ObservationFailed(format!("commit: {err}")))?;
            found.push(commit_info(&commit));
        }

        // We walked the whole window without ever meeting the anchor. Either
        // the history was rewritten or the window is simply too short to reach
        // it; either way the caller is told, and the bounded window is what we
        // have to offer.
        if previous_oid.is_some() && !reached_anchor {
            history_rewritten = true;
        }

        // revwalk yields newest first; reverse so events are emitted in the
        // order the commits actually happened.
        found.reverse();

        Ok(RangeWalk {
            commits: found,
            history_rewritten,
        })
    }

    /// The files one commit changed, with line counts, measured against its
    /// first parent (or the empty tree for a root commit).
    pub fn read_commit_changes(
        &self,
        repo: &Git2Repository,
        repository_id: &RepositoryId,
        sha: &str,
    ) -> Result<Vec<FileChangeRecord>, RepositoryError> {
        let oid = parse_oid(repo, sha)?;
        let commit = repo
            .find_commit(oid)
            .map_err(|_| RepositoryError::ObservationFailed(format!("unknown commit {sha}")))?;
        let tree = commit
            .tree()
            .map_err(|err| RepositoryError::ObservationFailed(format!("tree: {err}")))?;
        let parent_tree = commit.parent(0).ok().and_then(|parent| parent.tree().ok());

        let mut options = DiffOptions::new();
        let mut diff = repo
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut options))
            .map_err(|err| RepositoryError::ObservationFailed(format!("commit diff: {err}")))?;
        detect_renames(&mut diff)?;

        let now = chrono::Utc::now();
        let mut records = Vec::new();
        for delta in diff.deltas() {
            let change_type = map_delta_type(delta.status());
            let new_path = delta
                .new_file()
                .path()
                .map(normalize_path)
                .unwrap_or_default();
            let path = if change_type == FileChange::Deleted {
                delta
                    .old_file()
                    .path()
                    .map(normalize_path)
                    .unwrap_or(new_path.clone())
            } else {
                new_path
            };
            if path.is_empty() {
                continue;
            }
            let old_path = if change_type.has_old_path() {
                delta
                    .old_file()
                    .path()
                    .map(normalize_path)
                    .filter(|old| old != &path)
            } else {
                None
            };
            let fingerprint = if delta.new_file().id().is_zero() {
                DELETED_FINGERPRINT.to_string()
            } else {
                delta.new_file().id().to_string()
            };

            let (insertions, deletions) = line_counts(&diff, delta.new_file().id());

            let record = FileChangeRecord::new(
                repository_id.clone(),
                path,
                old_path,
                change_type,
                fingerprint,
                sha,
                now,
            )?
            .with_line_counts(insertions, deletions);
            records.push(record);
        }
        Ok(records)
    }

    /// Line-count diffs for the working tree, measured against `HEAD`.
    ///
    /// Unlike [`Self::read_worktree`], this computes per-file line statistics,
    /// which means materializing patches. That is the cost of the information;
    /// callers ask for it deliberately.
    pub fn read_diff(
        &self,
        repo: &Git2Repository,
        branch: Option<&str>,
    ) -> Result<Vec<director_domain::repository::DiffInfo>, RepositoryError> {
        let head_tree = repo.head().ok().and_then(|r| r.peel_to_tree().ok());
        let mut options = DiffOptions::new();
        options.include_untracked(true);

        // `tree_to_workdir_with_index` emulates `git diff HEAD`: it accounts
        // for the index, so a change that is staged, unstaged, or both is
        // reported once with its real extent.
        let mut diff = repo
            .diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut options))
            .map_err(|err| RepositoryError::ObservationFailed(format!("diff: {err}")))?;
        detect_renames(&mut diff)?;

        let now = chrono::Utc::now();
        let head_sha = self.read_head_commit(repo).unwrap_or_default();
        let mut diffs = Vec::new();
        for delta in diff.deltas() {
            let change_type = map_delta_type(delta.status());
            let old_path = delta.old_file().path().map(normalize_path);
            let new_path = delta
                .new_file()
                .path()
                .map(normalize_path)
                .unwrap_or_default();
            let (insertions, deletions) = line_counts(&diff, delta.new_file().id());
            diffs.push(director_domain::repository::DiffInfo {
                old_path: old_path.filter(|p| !p.is_empty() && change_type.has_old_path()),
                new_path,
                change_type,
                insertions,
                deletions,
                commit_sha: head_sha.clone(),
                branch: branch.map(str::to_string),
                observed_at: now,
            });
        }
        Ok(diffs)
    }
}

/// Enable rename and copy detection on a freshly created diff, mutating it in
/// place. Renames are not reported by a raw diff; they are found afterwards.
fn detect_renames(diff: &mut Diff) -> Result<(), RepositoryError> {
    let mut find = git2::DiffFindOptions::new();
    find.renames(true).copies(true).renames_from_rewrites(true);
    diff.find_similar(Some(&mut find))
        .map_err(|err| RepositoryError::ObservationFailed(format!("find similar: {err}")))
}

/// Map a git2 delta status onto Director's change vocabulary.
fn map_delta_type(status: Delta) -> FileChange {
    match status {
        Delta::Added => FileChange::Added,
        Delta::Modified => FileChange::Modified,
        Delta::Deleted => FileChange::Deleted,
        Delta::Renamed => FileChange::Renamed,
        Delta::Copied => FileChange::Copied,
        Delta::Untracked => FileChange::Untracked,
        Delta::Ignored => FileChange::Untracked,
        Delta::Conflicted => FileChange::Conflicted,
        Delta::Unmodified => FileChange::Modified,
        Delta::Typechange => FileChange::Modified,
        Delta::Unreadable => FileChange::Modified,
    }
}

/// Total added and removed lines among the deltas of `diff` matching `oid`.
///
/// Diffs are keyed by the new-side object id, which uniquely identifies the
/// content a delta landed on.
fn line_counts(diff: &Diff, oid: Oid) -> (u32, u32) {
    let mut insertions = 0u32;
    let mut deletions = 0u32;
    for (index, delta) in diff.deltas().enumerate() {
        if delta.new_file().id() != oid {
            continue;
        }
        if let Ok(Some(patch)) = git2::Patch::from_diff(diff, index) {
            if let Ok((added, removed, _context)) = patch.line_stats() {
                insertions += added as u32;
                deletions += removed as u32;
            }
        }
    }
    (insertions, deletions)
}

/// Build the domain commit record from a git2 commit.
fn commit_info(commit: &Commit) -> CommitInfo {
    let parents: Vec<String> = commit.parent_ids().map(|oid| oid.to_string()).collect();
    CommitInfo {
        sha: commit.id().to_string(),
        summary: commit.summary().ok().flatten().unwrap_or("").to_string(),
        author: signature_display(commit.author()).to_string(),
        committed_at: commit_time(&commit.committer()),
        parents,
        committer: signature_display(commit.committer()).to_string(),
        message: commit.message_raw().unwrap_or("").to_string(),
    }
}

/// Render a signature the way git does: `Name <email>`.
fn signature_display(signature: Signature) -> String {
    match (signature.name(), signature.email()) {
        (Ok(name), Ok(email)) => format!("{name} <{email}>"),
        (Ok(name), Err(_)) => name.to_string(),
        _ => String::new(),
    }
}

/// Convert a git2 time to a UTC timestamp.
///
/// The offset is dropped deliberately: Director records one canonical instant,
/// and comparing timestamps across machines with different local offsets is
/// only safe if they are all UTC.
fn commit_time(signature: &Signature) -> chrono::DateTime<chrono::Utc> {
    let time = signature.when();
    chrono::DateTime::<chrono::Utc>::from_timestamp(time.seconds(), 0)
        .unwrap_or_else(chrono::Utc::now)
}

/// Parse a hex SHA, accepting git's abbreviations.
fn parse_oid(repo: &Git2Repository, sha: &str) -> Result<Oid, RepositoryError> {
    Oid::from_str(sha)
        .or_else(|_| repo.revparse_single(sha).map(|obj| obj.id()))
        .map_err(|_| RepositoryError::ObservationFailed(format!("not a commit sha: {sha}")))
}

/// Repository-relative, forward-slash separated, no trailing dot artifacts.
fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Abbreviate a full SHA for display. Storage always keeps all 40 characters.
pub fn short_sha(sha: &str) -> &str {
    sha.get(..SHORT_SHA_LEN).unwrap_or(sha)
}

/// Whether a path looks like a conflict marker was left in it. Exposed for the
/// service, which surfaces conflicts as needing attention rather than hiding
/// them.
#[allow(dead_code)]
pub fn has_conflicts(repo: &Git2Repository) -> bool {
    repo.statuses(None)
        .map(|statuses| {
            statuses
                .iter()
                .any(|s| s.status().contains(Status::CONFLICTED))
        })
        .unwrap_or(false)
}
