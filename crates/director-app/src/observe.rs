//! The OBSERVE step of Orqyn's loop.
//!
//! This is the one place Orqyn looks at a repository and records what it found.
//! It composes two layers that Phase 2 and Phase 5 built independently and that
//! had never been wired together:
//!
//! - [`GitService`] (director-adapters) reads the live repository through git2
//!   and persists the *raw* observation — the repository record, its snapshot,
//!   and the event log — under a JSON store. This is what makes change
//!   detection possible: it remembers the last commit it saw.
//! - [`Store`] (director-store) holds Orqyn's *normalized belief* about the
//!   project in SQLite. This is what planning and assignment will read.
//!
//! The two are deliberately not the same thing. The git observation is
//! evidence: it is what git said, at a time, verbatim. The normalized state is
//! a belief Orqyn holds between observations, in a shape chosen for its own
//! decisions rather than for fidelity to git. Writing one from the other is the
//! whole job of this module, and it is the first code anywhere in the workspace
//! that opens a [`Store`].
//!
//! ## What a round does
//!
//! 1. Ensure the project and its repository are registered, so Orqyn has
//!    something to hang the observation off.
//! 2. Sync the repository against git. This is the actual observation: it
//!    re-reads the working tree, compares against the last snapshot, and
//!    reports what changed. Registering first is what gives the sync a
//!    baseline to compare against.
//! 3. Fold the resulting snapshot into a [`StoredProjectState`] and write it
//!    to the store.
//!
//! ## Where "changed" comes from
//!
//! The store bumps `state_version` on every write, so the version is a write
//! counter, not a change signal — a version comparison would report every
//! observation as a change. The authoritative answer is the git layer's
//! [`SyncResult`], which compares the snapshot against the previous one
//! deterministically. That is what the loop branches on.
//!
//! ## What it deliberately does not do
//!
//! No planning, no assignment, no verification. A round of OBSERVE changes
//! what Orqyn *knows* and nothing else. Keeping it that way is what will make
//! the later steps testable in isolation: each one consumes the state this
//! produces.
//!
//! The one exception is checkpointing, and it is an exception because a
//! checkpoint *is* what Orqyn just learned, written for a reader that has not
//! arrived yet. The checkpoint half of the round lives in
//! [`crate::checkpoint`] rather than here, so the observation stays about
//! recording evidence and the checkpointing stays about preserving a resumption
//! document; the two are one round, but they are not one concern.

use director_adapters::git::GitService;
use director_domain::ids::{IdGenerator, ProjectId, RepositoryId};
use director_domain::project::Project;
use director_domain::providers::MemoryProvider;
use director_domain::repository::{ProjectStateSnapshot, SyncStatus};
use director_domain::state::TestResults;
use director_domain::store::StoredProjectState;
use director_domain::{ProjectRepository, ProjectStateRepository};
use director_store::Store;

use crate::ObserveError;

/// The outcome of one OBSERVE round, reported to the caller rather than logged
/// and dropped — the loop's later steps will branch on this.
///
/// `PartialEq` but not `Eq`: it carries [`Checkpoint`]s, whose
/// `progress_fraction` is an `f32`, so total equality is not well defined.
/// Comparisons are for test assertions.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    /// The project the observation is about.
    pub project_id: ProjectId,
    /// The repository it was taken of.
    pub repository_id: RepositoryId,
    /// The commit `HEAD` pointed at, empty for a repository with no commits.
    pub head_commit: String,
    /// The branch, or `None` under a detached HEAD.
    pub branch: Option<String>,
    /// Whether the working tree was clean at observation time.
    pub working_tree_clean: bool,
    /// How many observations Orqyn has now made of this project.
    pub observation_version: u64,
    /// The state version after this observation.
    pub state_version: u64,
    /// Whether this observation recorded a real change — a new commit, a moved
    /// branch, or a changed working tree. A restatement is not an error — a
    /// loop polling an idle repository should see a steady stream of `false` —
    /// but it is the signal the loop uses to decide whether anything is worth
    /// planning against, and whether any in-flight work is worth
    /// checkpointing.
    pub changed: bool,
    /// Whether this was the first observation Orqyn has ever made of this
    /// project. The first one is a baseline rather than a change: Orqyn arrived
    /// and found the repository as it was, it did not watch it get there.
    pub first_observation: bool,
    /// The checkpoints this round wrote for in-progress tasks, if any. Empty
    /// unless the observation recorded a real change and the project has work
    /// in flight — a quiet round checkpoints nothing. This is the Phase 12
    /// activation: the recovery lineage advances on the same evidence that
    /// makes the loop re-plan.
    pub checkpoints: Vec<director_domain::checkpoint::Checkpoint>,
}

/// The registered view of the repository, reported alongside the normalized
/// state so a caller can see both layers at once without a second round trip.
#[derive(Debug, Clone)]
pub struct Observed {
    /// The normalized state now held in Orqyn's store.
    pub state: StoredProjectState,
    /// The raw view the git service recorded.
    pub view: director_adapters::git::RepositoryStatusView,
    /// The outcome of the underlying git sync: what changed, and how many
    /// events the observation created.
    pub sync: director_domain::repository::SyncResult,
    /// Whether this round registered the repository — i.e. whether it was
    /// Orqyn's first ever look at it. This is reported by the registration
    /// itself rather than inferred from the sync status, because the two are
    /// not the same thing: [`SyncStatus::Baseline`] fires only for a
    /// repository with no commits to walk, so a repository that already has
    /// history syncs as [`SyncStatus::Synced`] on its very first observation.
    /// The loop's later steps branch on arrival-versus-change, so the
    /// distinction has to be right.
    pub first_observation: bool,
    /// The checkpoints the round wrote for in-progress tasks. Empty on a quiet
    /// round or a round with no work in flight.
    pub checkpoints: Vec<director_domain::checkpoint::Checkpoint>,
}

/// Run one OBSERVE round for a project's repository.
///
/// `local_path` is the working tree to look at. Registering the project and
/// repository is idempotent: a second call on the same ids reuses them rather
/// than erroring, which is what makes this safe to call on every loop tick.
///
/// `memory` is the semantic-memory substrate the round publishes checkpoints
/// to, best-effort. It is taken as the domain [`MemoryProvider`] trait so this
/// step never learns which substrate is answering.
pub async fn observe<M: MemoryProvider>(
    store: &Store,
    git: &GitService,
    ids: &mut IdGenerator,
    memory: &M,
    project_id: &ProjectId,
    repository_id: &RepositoryId,
    local_path: &str,
) -> Result<Observed, ObserveError> {
    observe_at(
        store,
        git,
        ids,
        memory,
        project_id,
        repository_id,
        local_path,
        chrono::Utc::now(),
    )
    .await
}

/// [`observe`], with the observation time supplied by the caller. Exposed so a
/// test can make two rounds distinguishable and assert that observation
/// versions advance; production passes [`chrono::Utc::now`].
#[allow(clippy::too_many_arguments)]
pub async fn observe_at<M: MemoryProvider>(
    store: &Store,
    git: &GitService,
    ids: &mut IdGenerator,
    memory: &M,
    project_id: &ProjectId,
    repository_id: &RepositoryId,
    local_path: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Observed, ObserveError> {
    // 1. Ensure the project exists. A project is a root, not a container: the
    //    tasks reference it by id and are never nested under it, so creating it
    //    here is enough for the observation to have something to hang off. A
    //    duplicate is the expected case on a second tick, not an error.
    ensure_project(store, project_id, local_path).await?;

    // 2. Register the repository if it has never been observed. This is not an
    //    optimization: an unregistered repository has no stored snapshot, so a
    //    sync would have no baseline to compare against. Establishing the
    //    baseline is what makes the *next* observation able to detect a change.
    let first = git.status(repository_id).is_err();
    if first {
        git.initialize_repository(project_id.clone(), repository_id.clone(), local_path)
            .await?;
    }

    // 3. The observation itself. This re-reads the working tree — unlike
    //    `status`, which only reports what a previous call stored — compares
    //    against the last snapshot, and reports what changed.
    let sync = git.sync_repository(repository_id).await?;
    let view = git.status(repository_id)?;

    // 4. Fold the raw snapshot into the normalized belief and store it. The
    //    store bumps the version on every write, so the *change* signal is
    //    taken from the sync result rather than from the version.
    let prior = store.project_state().get_project_state(project_id).await?;
    let (prior_version, prior_observed) = prior
        .as_ref()
        .map(|p| (p.state_version, p.observation_version))
        .unwrap_or((1, 0));
    let state = StoredProjectState {
        project_id: project_id.clone(),
        repository_id: Some(repository_id.clone()),
        branch: view.snapshot.branch.clone(),
        head_commit: view.snapshot.head_commit.clone(),
        working_tree_clean: view.snapshot.working_tree_clean,
        observation_version: prior_observed + 1,
        last_observed_at: now,
        state_version: prior_version,
    };
    let stored = store.project_state().update_project_state(&state).await?;

    // 5. Checkpoint the in-flight work. This is the Phase 12 activation: the
    //    observation is the one place that holds durable progress evidence, so
    //    it is the step that advances the recovery lineage. A quiet round writes
    //    nothing, which is what keeps an idle repository from accumulating
    //    restatements. A conflict here is reported rather than swallowed — a
    //    checkpoint the round believed it wrote but did not is exactly the thing
    //    a caller must learn about.
    let checkpoints = crate::checkpoint::checkpoint_round(
        store,
        ids,
        memory,
        project_id,
        &stored,
        &sync,
        &view.snapshot.working_tree,
        now,
    )
    .await?;

    Ok(Observed {
        view,
        sync,
        state: stored,
        first_observation: first,
        checkpoints: checkpoints.written,
    })
}

/// Summarize one round for a caller that wants the change signal without the
/// raw layers.
pub fn summary(
    project_id: &ProjectId,
    repository_id: &RepositoryId,
    observed: &Observed,
) -> Observation {
    Observation {
        project_id: project_id.clone(),
        repository_id: repository_id.clone(),
        head_commit: observed.state.head_commit.clone(),
        branch: observed.state.branch.clone(),
        working_tree_clean: observed.state.working_tree_clean,
        observation_version: observed.state.observation_version,
        state_version: observed.state.state_version,
        // Arrival is not a change: Orqyn observed the repository as it already
        // was, so the first round reports a baseline. Only a later sync that
        // recorded a new fact counts as a change.
        changed: !observed.first_observation && observed.sync.status == SyncStatus::Synced,
        first_observation: observed.first_observation,
        checkpoints: observed.checkpoints.clone(),
    }
}

/// Create the project if it is absent. Idempotent.
async fn ensure_project(
    store: &Store,
    project_id: &ProjectId,
    local_path: &str,
) -> Result<(), ObserveError> {
    match store.projects().get_project(project_id).await {
        Ok(_) => Ok(()),
        Err(director_domain::StoreError::NotFound(_)) => {
            let project = Project::new(project_id.clone(), project_id.as_str(), local_path);
            store.projects().create_project(&project).await?;
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

/// The test results for a snapshot, when the observation recorded any.
///
/// This exists to give the loop's later VERIFY step a seam: the state Orqyn
/// holds is what verification reads, and test results are part of that state.
/// It is unused until that step exists, and is held here rather than deleted so
/// the wiring does not have to be rediscovered.
#[allow(dead_code)]
pub fn test_results(_snapshot: &ProjectStateSnapshot) -> Option<TestResults> {
    // The git snapshot does not yet carry test results — they are produced by
    // verification, not by observation. When the VERIFY step lands it will
    // populate this from the store rather than from the snapshot.
    None
}
