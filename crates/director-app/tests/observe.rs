//! Integration tests for the OBSERVE step against a real git repository and a
//! real SQLite store.
//!
//! These are the tests that prove the wiring, and they are integration tests
//! for a specific reason: every layer underneath has its own suite, but until
//! this crate nothing composed them. A unit test over a mock could only ever
//! confirm that `observe` calls the methods it was mocked to expect; it cannot
//! show that the git service's snapshot shape and the store's
//! `StoredProjectState` actually line up, which is the whole risk of this
//! crate. So each test builds a genuine git repository, commits real content,
//! and observes it through a real store on disk.
//!
//! The repositories are temporary and deleted when the test ends.

use std::process::Command;

use director_adapters::git::GitService;
use director_app::observe::{observe, observe_at, summary};
use director_domain::ids::{ProjectId, RepositoryId};
use director_domain::{ProjectRepository, ProjectStateRepository};
use director_store::Store;

/// A temporary scratch directory holding a real git repository with one commit,
/// plus a separate directory for the git service's own state.
struct Scratch {
    repo_dir: std::path::PathBuf,
    state_dir: std::path::PathBuf,
    _tmp: tempfile::TempDir,
}

impl Scratch {
    fn new() -> Self {
        let tmp = tempfile::TempDir::new().expect("a temp dir");
        let repo_dir = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("repo dir");

        let git = || {
            let mut cmd = Command::new("git");
            cmd.current_dir(&repo_dir);
            cmd
        };
        // A standalone commit needs an identity; the test environment has none.
        git().args(["init", "-q"]).status().expect("git init");
        git()
            .args(["config", "user.name", "Orqyn Tests"])
            .status()
            .expect("set name");
        git()
            .args(["config", "user.email", "tests@orqyn.invalid"])
            .status()
            .expect("set email");
        git()
            .args(["config", "commit.gpgsign", "false"])
            .status()
            .expect("disable signing");

        let state_dir = tmp.path().join("orqyn-state");
        std::fs::create_dir_all(&state_dir).expect("state dir");

        Scratch {
            repo_dir,
            state_dir,
            _tmp: tmp,
        }
    }

    /// Write a file and commit it, returning the short sha git reported.
    fn commit(&self, path: &str, contents: &str, message: &str) -> String {
        let file = self.repo_dir.join(path);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).expect("parent dir");
        }
        std::fs::write(&file, contents).expect("write file");
        let status = Command::new("git")
            .current_dir(&self.repo_dir)
            .args(["add", path])
            .status()
            .expect("git add");
        assert!(status.success(), "git add should succeed");
        let status = Command::new("git")
            .current_dir(&self.repo_dir)
            .args(["commit", "-q", "-m", message])
            .status()
            .expect("git commit");
        assert!(status.success(), "git commit should succeed");
        let sha = Command::new("git")
            .current_dir(&self.repo_dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        assert!(sha.status.success(), "rev-parse HEAD");
        String::from_utf8(sha.stdout)
            .expect("sha is utf-8")
            .trim()
            .to_string()
    }

    /// A store in a sibling temp directory, deleted when the test ends.
    async fn store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().expect("a temp dir for the store");
        let store = Store::open(dir.path().join("orqyn.db"))
            .await
            .expect("store opens");
        (store, dir)
    }

    fn service(&self) -> GitService {
        GitService::new(&self.state_dir).expect("git service")
    }

    fn path(&self) -> String {
        self.repo_dir.to_string_lossy().replace('\\', "/")
    }
}

const PROJECT: &str = "PROJ-1";
const REPO: &str = "REPO-1";

fn ids() -> (ProjectId, RepositoryId) {
    (
        ProjectId::from_string(PROJECT),
        RepositoryId::from_string(REPO),
    )
}

#[tokio::test]
async fn a_first_observation_registers_and_records_the_head() {
    let scratch = Scratch::new();
    let sha = scratch.commit("README.md", "# checkout-service\n", "initial");
    let (store, _store_dir) = Scratch::store().await;
    let git = scratch.service();
    let (project, repo) = ids();

    let observed = observe(&store, &git, &project, &repo, &scratch.path())
        .await
        .expect("the first observation");

    // The project was created as a side effect of observing it.
    store
        .projects()
        .get_project(&project)
        .await
        .expect("the project exists");

    assert_eq!(observed.state.head_commit, sha);
    assert!(observed.state.working_tree_clean, "nothing is uncommitted");
    assert_eq!(
        observed.state.observation_version, 1,
        "the first observation"
    );
    assert_eq!(
        observed.view.repository.id, repo,
        "the raw view is reported alongside the state"
    );

    // The first observation establishes a baseline rather than detecting a
    // change: Orqyn arrived and found the repository as it was.
    let summary = summary(&project, &repo, &observed);
    assert!(summary.first_observation);
    assert!(
        !summary.changed,
        "the first observation is a baseline, not a change"
    );
}

#[tokio::test]
async fn a_repeated_observation_of_an_unchanged_repo_reports_no_change() {
    let scratch = Scratch::new();
    scratch.commit("README.md", "# checkout-service\n", "initial");
    let (store, _store_dir) = Scratch::store().await;
    let git = scratch.service();
    let (project, repo) = ids();
    let path = scratch.path();

    observe(&store, &git, &project, &repo, &path)
        .await
        .expect("first");
    let again = observe(&store, &git, &project, &repo, &path)
        .await
        .expect("second");

    assert!(
        !summary(&project, &repo, &again).changed,
        "observing the same state again is a restatement, not a change"
    );
    assert_eq!(
        again.state.observation_version, 2,
        "the clock still advances"
    );
}

#[tokio::test]
async fn a_new_commit_is_observed_as_a_change() {
    let scratch = Scratch::new();
    scratch.commit("README.md", "# checkout-service\n", "initial");
    let (store, _store_dir) = Scratch::store().await;
    let git = scratch.service();
    let (project, repo) = ids();
    let path = scratch.path();

    let first = observe(&store, &git, &project, &repo, &path)
        .await
        .expect("first observation");

    let sha = scratch.commit("src/lib.rs", "pub fn retry() {}\n", "add the retry helper");
    let second = observe(&store, &git, &project, &repo, &path)
        .await
        .expect("second observation");

    let summary = summary(&project, &repo, &second);
    assert!(summary.changed, "a new commit is a meaningful change");
    assert!(!summary.first_observation);
    assert_eq!(second.state.head_commit, sha);
    assert_eq!(second.state.observation_version, 2);
    assert!(
        second.state.state_version > first.state.state_version,
        "the state version advanced"
    );
    assert!(second.sync.events_created > 0, "the sync recorded events");
}

#[tokio::test]
async fn a_dirty_working_tree_is_recorded_as_unclean() {
    let scratch = Scratch::new();
    scratch.commit("README.md", "# checkout-service\n", "initial");
    let (store, _store_dir) = Scratch::store().await;
    let git = scratch.service();
    let (project, repo) = ids();
    let path = scratch.path();

    observe(&store, &git, &project, &repo, &path)
        .await
        .expect("clean observation");

    let untracked = scratch.repo_dir.join("src/lib.rs");
    std::fs::create_dir_all(untracked.parent().expect("a parent dir")).expect("parent dir");
    std::fs::write(&untracked, "uncommitted\n").expect("an untracked file");
    let dirty = observe(&store, &git, &project, &repo, &path)
        .await
        .expect("observation with a dirty tree");

    assert!(
        !dirty.state.working_tree_clean,
        "the uncommitted change is visible to the observation"
    );
    assert!(
        summary(&project, &repo, &dirty).changed,
        "a dirty tree is a change from a clean one"
    );
}

#[tokio::test]
async fn a_directory_that_is_not_a_repository_is_refused() {
    let tmp = tempfile::TempDir::new().expect("a temp dir");
    let not_a_repo = tmp.path().join("plain");
    std::fs::create_dir_all(&not_a_repo).expect("dir");

    let (store, _store_dir) = Scratch::store().await;
    let git = GitService::new(tmp.path().join("state")).expect("git service");
    let (project, repo) = ids();
    let path = not_a_repo.to_string_lossy().replace('\\', "/");

    let err = observe(&store, &git, &project, &repo, &path)
        .await
        .unwrap_err();

    // The failure is a git failure, and it is reported rather than swallowed —
    // a directory that is not a repository must never be silently accepted.
    assert!(
        matches!(err, director_app::ObserveError::Git(_)),
        "got {err:?}"
    );
    // And nothing was recorded for a project that could not be observed.
    assert!(store
        .project_state()
        .get_project_state(&project)
        .await
        .expect("the query")
        .is_none());
}

#[tokio::test]
async fn the_observation_clock_is_distinguishable_across_rounds() {
    let scratch = Scratch::new();
    scratch.commit("README.md", "# checkout-service\n", "initial");
    let (store, _store_dir) = Scratch::store().await;
    let git = scratch.service();
    let (project, repo) = ids();
    let path = scratch.path();

    // observe_at exists so two rounds can be told apart by time; here it is
    // used to confirm the clock advances even when the state does not.
    let t0 = chrono::Utc::now();
    let a = observe_at(&store, &git, &project, &repo, &path, t0)
        .await
        .expect("first");
    std::thread::sleep(std::time::Duration::from_millis(10));
    let b = observe_at(&store, &git, &project, &repo, &path, chrono::Utc::now())
        .await
        .expect("second");

    assert!(a.state.last_observed_at < b.state.last_observed_at);
    assert_eq!(a.state.observation_version + 1, b.state.observation_version);
    assert!(!summary(&project, &repo, &b).changed);
}
