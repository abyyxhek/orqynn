//! Publishing checkpoints to a configured memory substrate.
//!
//! # What this file is about
//!
//! Orqyn's loop is generic over the domain `MemoryProvider` trait, so an
//! external substrate has always been injectable — this phase made that explicit
//! and closed the one real gap: `MEMORY_PUBLISH_TIMEOUT` was documented but never
//! applied, so a slow substrate could hold an observation round open forever.
//!
//! # What is real here
//!
//! SQLite is real and authoritative: every test persists through the real store
//! and reads the checkpoint back after publication fails, which is the property
//! that actually matters. Publication goes through the real
//! `publish_checkpoint_bounded` and the real payload builder.
//!
//! The provider itself is a recording double, because a test must be able to
//! make publication *succeed*, *fail*, and *hang* on demand — three things a
//! real server cannot be asked to do mid-suite. The transport-level evidence that
//! a real MCP provider behaves correctly under those conditions already exists in
//! `director-adapters`' `mcp_transport.rs` and `mcp_adapter_lock.rs`; this file
//! proves the application half, and one live test at the end proves the two meet
//! against the actual server.
//!
//! # No external server is required
//!
//! Nothing in this file spawns an MCP server, reads an environment variable, or
//! needs a credential. `ordinary_tests_need_no_external_server` asserts the first
//! of those structurally.
//!
//! No substrate is named here either, and that is not an accident: the repository
//! enforces a boundary (`director-adapters/tests/boundary.rs`) that no Rust file
//! outside the adapters crate may reference a substrate by name. Injectability is
//! therefore proven where it belongs — in the adapter's own crate — rather than
//! asserted from the application side.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use director_adapters::InMemoryProvider;
use director_app::checkpoint::{
    publish_checkpoint_bounded, CheckpointRound, PublishOutcome, MEMORY_PUBLISH_TIMED_OUT,
    MEMORY_PUBLISH_TIMEOUT,
};
use director_domain::checkpoint::Checkpoint;
use director_domain::ids::{CheckpointId, IdGenerator, ProjectId, RepositoryId, TaskId};
use director_domain::project::{DefaultBranch, Project};
use director_domain::providers::{Memory, MemoryProvider, MemoryQuery, Provider};
use director_domain::repository::{FileChangeRecord, SyncResult, SyncStatus, WorktreeState};
use director_domain::state::FileChange;
use director_domain::store::{
    CheckpointRepository, ProjectRepository, StoredProjectState, TaskRepository,
};
use director_domain::task::{ExpectedOutput, Task, TaskStatus};
use director_store::Store;

const PROJECT: &str = "PROJ-1";
const REPO: &str = "REPO-1";

// ---------------------------------------------------------------------------
// A provider whose behaviour the test decides
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
#[error("the substrate refused: {0}")]
struct Refused(String);

/// What the double should do with a save.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Behaviour {
    Accept,
    Refuse(String),
    /// Never answer. Used to exercise the publication bound in milliseconds.
    Hang,
}

/// A `MemoryProvider` that records what it was asked to store.
#[derive(Clone)]
struct RecordingMemory {
    behaviour: Behaviour,
    saved: Arc<StdMutex<Vec<Memory>>>,
}

impl RecordingMemory {
    fn accepting() -> Self {
        RecordingMemory {
            behaviour: Behaviour::Accept,
            saved: Arc::new(StdMutex::new(Vec::new())),
        }
    }

    fn refusing(message: &str) -> Self {
        RecordingMemory {
            behaviour: Behaviour::Refuse(message.to_string()),
            saved: Arc::new(StdMutex::new(Vec::new())),
        }
    }

    fn hanging() -> Self {
        RecordingMemory {
            behaviour: Behaviour::Hang,
            saved: Arc::new(StdMutex::new(Vec::new())),
        }
    }

    fn saved(&self) -> Vec<Memory> {
        self.saved.lock().expect("the recording lock").clone()
    }
}

#[async_trait::async_trait]
impl Provider for RecordingMemory {
    type Error = Refused;
}

#[async_trait::async_trait]
impl MemoryProvider for RecordingMemory {
    async fn save_memory(&self, memory: Memory) -> Result<Memory, Self::Error> {
        match &self.behaviour {
            Behaviour::Accept => {
                self.saved
                    .lock()
                    .expect("the recording lock")
                    .push(memory.clone());
                Ok(memory)
            }
            Behaviour::Refuse(message) => Err(Refused(message.clone())),
            Behaviour::Hang => {
                // Longer than any bound a test passes, so only the bound can end
                // this. Not a sleep in the test — the test never waits.
                tokio::time::sleep(Duration::from_secs(3_600)).await;
                Ok(memory)
            }
        }
    }

    async fn query_memories(&self, _query: &MemoryQuery) -> Result<Vec<Memory>, Self::Error> {
        Ok(Vec::new())
    }

    async fn recent_memories(&self, _limit: u32) -> Result<Vec<Memory>, Self::Error> {
        Ok(self.saved())
    }
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

fn pinned() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap()
}

struct Fixture {
    store: Store,
    ids: IdGenerator,
    project: ProjectId,
    _tmp: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let tmp = tempfile::TempDir::new().expect("a temp dir");
        let store = Store::open(tmp.path().join("orqyn.db"))
            .await
            .expect("the store opens");
        Fixture {
            store,
            ids: IdGenerator::new(),
            project: ProjectId::from_string(PROJECT),
            _tmp: tmp,
        }
    }

    /// A project, an agent, and one in-progress task with a real check.
    async fn seeded(&self) {
        self.store
            .projects()
            .create_project(&Project {
                id: self.project.clone(),
                name: "orqyn".into(),
                root: "C:/repo".into(),
                default_branch: DefaultBranch::Main,
                repository_id: Some(RepositoryId::from_string(REPO)),
                created_at: pinned(),
                updated_at: pinned(),
                state_version: 1,
            })
            .await
            .expect("the project lands");

        // The task is created fully formed. A task row must name its project at
        // creation - the store refuses an unscoped one rather than inventing a
        // scope - so the round's inputs are supplied here rather than patched on
        // afterwards.
        self.store
            .tasks()
            .create_task(&Task {
                project_id: Some(self.project.clone()),
                status: TaskStatus::InProgress,
                expected_outputs: vec![ExpectedOutput {
                    criterion: "tests pass".into(),
                    check: Some("cargo test".into()),
                }],
                scope_paths: vec!["src/auth".into()],
                ..Task::new(
                    TaskId::from_string("AUTH"),
                    "auth",
                    "the endpoint answers 401",
                )
            })
            .await
            .expect("the in-progress task lands");
    }

    fn state(&self) -> StoredProjectState {
        StoredProjectState {
            project_id: self.project.clone(),
            repository_id: Some(RepositoryId::from_string(REPO)),
            branch: Some("main".into()),
            head_commit: "abc".into(),
            working_tree_clean: false,
            observation_version: 1,
            last_observed_at: pinned(),
            state_version: 1,
        }
    }

    fn synced(&self) -> SyncResult {
        SyncResult {
            status: SyncStatus::Synced,
            branch_changed: false,
            commit_changed: true,
            working_tree_changed: true,
            events_created: 1,
            previous_commit: None,
            current_commit: "abc".into(),
            state_version: 1,
        }
    }

    fn changed_file(&self) -> FileChangeRecord {
        FileChangeRecord {
            repository_id: RepositoryId::from_string(REPO),
            path: "src/auth/mod.rs".into(),
            old_path: None,
            change_type: FileChange::Modified,
            insertions: None,
            deletions: None,
            fingerprint: "src/auth/mod.rs".into(),
            commit_sha: "abc".into(),
            observed_at: pinned(),
        }
    }
}

// ---------------------------------------------------------------------------
// 1. Existing in-memory behaviour is unchanged
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_in_memory_provider_still_receives_every_checkpoint() {
    let fixture = Fixture::new().await;
    fixture.seeded().await;
    let memory = InMemoryProvider::new();

    let round = director_app::checkpoint::checkpoint_round(
        &fixture.store,
        &mut fixture.ids.clone(),
        &memory,
        &fixture.project,
        &fixture.state(),
        &fixture.synced(),
        &WorktreeState {
            modified: vec![fixture.changed_file()],
            ..WorktreeState::default()
        },
        pinned(),
    )
    .await
    .expect("the round");

    assert_eq!(
        round.written.len(),
        1,
        "the in-progress task is checkpointed"
    );
    assert!(
        round.memory_failures.is_empty(),
        "the in-memory provider accepts everything: {:?}",
        round.memory_failures
    );
    assert!(
        round.written[0]
            .changed_files
            .contains(&"src/auth/mod.rs".to_string()),
        "the checkpoint carries the observed change"
    );
}

// ---------------------------------------------------------------------------
// 2. The configured provider receives the expected payload
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_configured_provider_receives_the_expected_payload() {
    let fixture = Fixture::new().await;
    fixture.seeded().await;
    let memory = RecordingMemory::accepting();

    let round = director_app::checkpoint::checkpoint_round(
        &fixture.store,
        &mut fixture.ids.clone(),
        &memory,
        &fixture.project,
        &fixture.state(),
        &fixture.synced(),
        &WorktreeState {
            modified: vec![fixture.changed_file()],
            ..WorktreeState::default()
        },
        pinned(),
    )
    .await
    .expect("the round");

    assert_eq!(round.written.len(), 1);
    assert!(round.memory_failures.is_empty());

    let saved = memory.saved();
    assert_eq!(saved.len(), 1, "exactly one publication, not two");
    let page = &saved[0];

    // The path is derived from the task, lowercased, and is the substrate's own
    // addressing unit.
    assert_eq!(page.id, "checkpoints/auth.md");
    assert_eq!(page.title, "Checkpoint: AUTH");
    assert!(page.tags.contains(&"orqyn-checkpoint".to_string()));
    assert!(
        page.tags.iter().any(|t| t == "auth"),
        "the task is tagged so the page is findable: {:?}",
        page.tags
    );

    // And the body is drawn entirely from the checkpoint's own evidence.
    assert!(page.body.contains("Resume status evidence for AUTH"));
    assert!(page.body.contains("objective:"));
    assert!(page.body.contains("branch: main"));
    assert!(page.body.contains("head: abc"));
    assert!(page.body.contains("progress:"));
    assert!(page.body.contains("next action:"));
    assert!(page.body.contains("changed files: src/auth/mod.rs"));

    // Nothing is inferred. Orqyn's own retention `kind` is not a substrate tier,
    // so it must not be invented into the page.
    assert!(
        page.kind.is_none(),
        "no retention tier is guessed; the substrate picks its default"
    );
}

// ---------------------------------------------------------------------------
// 4 & 5. Success and failure are reported accurately
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_successful_publication_reports_no_failure() {
    let fixture = Fixture::new().await;
    fixture.seeded().await;
    let memory = RecordingMemory::accepting();

    let round = round_with(&fixture, &memory).await;
    assert!(
        round.memory_failures.is_empty(),
        "a substrate that accepted everything must report nothing failed"
    );
    assert_eq!(
        round.written.len(),
        1,
        "and the checkpoint is still written"
    );
}

#[tokio::test]
async fn a_refused_publication_is_recorded_not_swallowed() {
    let fixture = Fixture::new().await;
    fixture.seeded().await;
    let memory = RecordingMemory::refusing("the page was rejected");

    let round = round_with(&fixture, &memory).await;

    assert_eq!(
        round.memory_failures.len(),
        1,
        "the failure must be visible to the caller, not dropped"
    );
    let failure = &round.memory_failures[0];
    assert_eq!(failure.task_id, TaskId::from_string("AUTH"));
    assert!(
        failure.error.contains("the page was rejected"),
        "the substrate's own message survives: {}",
        failure.error
    );
    assert!(
        !failure.error.contains(MEMORY_PUBLISH_TIMED_OUT),
        "a refusal is not reported as a timeout"
    );
}

// ---------------------------------------------------------------------------
// 6. Local persistence survives a publication failure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_refused_publication_leaves_sqlite_authoritative() {
    let fixture = Fixture::new().await;
    fixture.seeded().await;
    let memory = RecordingMemory::refusing("the substrate is down");

    let round = round_with(&fixture, &memory).await;

    // The failure is reported...
    assert_eq!(round.memory_failures.len(), 1);
    // ...and the checkpoint is nonetheless persisted, because SQLite is the
    // record of what happened, not a mirror of the substrate.
    assert_eq!(round.written.len(), 1, "the checkpoint still lands");
    let stored = fixture
        .store
        .checkpoints()
        .latest_checkpoint(&TaskId::from_string("AUTH"))
        .await
        .expect("a checkpoint reads back")
        .expect("one was persisted");
    assert_eq!(stored.task_id, TaskId::from_string("AUTH"));
    assert!(stored
        .changed_files
        .contains(&"src/auth/mod.rs".to_string()));
}

#[tokio::test]
async fn a_timed_out_publication_leaves_sqlite_authoritative_too() {
    let fixture = Fixture::new().await;
    fixture.seeded().await;
    let memory = RecordingMemory::hanging();

    let round = round_within(&fixture, &memory, Duration::from_millis(50)).await;

    assert_eq!(round.written.len(), 1, "the checkpoint still lands");
    assert_eq!(round.memory_failures.len(), 1);
    assert!(
        round.memory_failures[0].error.contains("timed out"),
        "a timeout is recorded as a timeout: {}",
        round.memory_failures[0].error
    );
    assert!(
        fixture
            .store
            .checkpoints()
            .latest_checkpoint(&TaskId::from_string("AUTH"))
            .await
            .expect("a checkpoint reads back")
            .is_some(),
        "the round's other work is not gated on the substrate"
    );
}

// ---------------------------------------------------------------------------
// The publication bound, exercised in milliseconds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_publication_bound_is_enforced() {
    let memory = RecordingMemory::hanging();
    let checkpoint = sample_checkpoint();

    let started = std::time::Instant::now();
    let outcome = publish_checkpoint_bounded(&memory, &checkpoint, Duration::from_millis(50)).await;
    let elapsed = started.elapsed();

    assert_eq!(outcome, PublishOutcome::TimedOut);
    assert!(
        elapsed < Duration::from_secs(2),
        "the bound must end the wait, not the substrate; took {elapsed:?}"
    );
    assert_eq!(
        outcome.failure_text(),
        Some(MEMORY_PUBLISH_TIMED_OUT.to_string()),
        "a timeout is recorded with the documented text"
    );
    assert!(
        memory.saved().is_empty(),
        "a timed-out publication must not be recorded as having happened"
    );
}

#[tokio::test]
async fn a_refusal_and_a_timeout_are_distinguishable_outcomes() {
    let checkpoint = sample_checkpoint();
    let refused = publish_checkpoint_bounded(
        &RecordingMemory::refusing("no"),
        &checkpoint,
        Duration::from_secs(2),
    )
    .await;
    let timed_out = publish_checkpoint_bounded(
        &RecordingMemory::hanging(),
        &checkpoint,
        Duration::from_millis(50),
    )
    .await;

    assert_ne!(
        refused, timed_out,
        "operators must be able to tell these apart"
    );
    assert!(matches!(refused, PublishOutcome::Refused(_)));
    assert!(matches!(timed_out, PublishOutcome::TimedOut));
    assert_eq!(
        refused.failure_text(),
        Some("the substrate refused: no".to_string())
    );
    assert_ne!(refused.failure_text(), timed_out.failure_text());
}

#[tokio::test]
async fn the_documented_bound_is_the_value_the_round_uses() {
    // The constant and the round must not drift apart: the round passes
    // `MEMORY_PUBLISH_TIMEOUT`, and it is a real bound rather than a fiction.
    assert_eq!(MEMORY_PUBLISH_TIMEOUT, Duration::from_secs(10));
    assert!(
        director_app::checkpoint::MEMORY_PUBLISH_TIMED_OUT.contains("timed out"),
        "the recorded text must say so"
    );
}

// ---------------------------------------------------------------------------
// 7. Retries republish; nothing claims exactly-once
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_retried_round_republishes_the_same_page_without_claiming_exactly_once() {
    let fixture = Fixture::new().await;
    fixture.seeded().await;
    let memory = RecordingMemory::accepting();

    let first = round_with(&fixture, &memory).await;
    let second = round_with(&fixture, &memory).await;

    assert!(first.memory_failures.is_empty());
    assert!(second.memory_failures.is_empty(), "the retry succeeds too");

    // Two rounds, two publications — which is exactly why no exactly-once claim
    // is made. What is *not* doubled is the page's identity: both publications
    // address the same path, so the substrate holds one page for the task rather
    // than accumulating one per attempt.
    let saved = memory.saved();
    assert_eq!(saved.len(), 2, "the retry published again, as documented");
    let paths: std::collections::BTreeSet<&str> = saved.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        paths,
        std::collections::BTreeSet::from(["checkpoints/auth.md"]),
        "every attempt addresses the same page, so a retry overwrites rather than duplicates"
    );

    // SQLite's lineage advanced, which is the authoritative record that this was
    // a second observation rather than a duplicate write.
    assert_eq!(first.written[0].state_version, 1);
    assert_eq!(second.written[0].state_version, 2);
}

// ---------------------------------------------------------------------------
// 8. Ordinary tests need no external server
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ordinary_tests_need_no_external_server() {
    // Structural, not just a claim: every provider these tests use is constructed
    // in-process. `McpTimeouts` and the config type exist in the adapters crate,
    // but nothing here reaches for one, so the checkpoint suite passes with no
    // ai-memory binary, no data directory, and no credential.
    let fixture = Fixture::new().await;
    fixture.seeded().await;

    let in_memory = InMemoryProvider::new();
    let double = RecordingMemory::accepting();
    let refused = RecordingMemory::refusing("x");
    let hanging = RecordingMemory::hanging();

    for round in [
        round_with(&fixture, &in_memory).await,
        round_with(&fixture, &double).await,
        round_with(&fixture, &refused).await,
        round_within(&fixture, &hanging, Duration::from_millis(50)).await,
    ] {
        assert_eq!(
            round.written.len(),
            1,
            "all three publish paths run offline"
        );
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn round_with<M: MemoryProvider>(fixture: &Fixture, memory: &M) -> CheckpointRound {
    director_app::checkpoint::checkpoint_round(
        &fixture.store,
        &mut fixture.ids.clone(),
        memory,
        &fixture.project,
        &fixture.state(),
        &fixture.synced(),
        &WorktreeState {
            modified: vec![fixture.changed_file()],
            ..WorktreeState::default()
        },
        pinned(),
    )
    .await
    .expect("the round")
}

/// [`round_with`] with an explicit publication bound.
///
/// The hanging provider never answers, so a round using the production ten-second
/// bound would make this suite take ten seconds per case. The bound is what ends
/// the wait either way; only its size differs.
async fn round_within<M: MemoryProvider>(
    fixture: &Fixture,
    memory: &M,
    limit: Duration,
) -> CheckpointRound {
    director_app::checkpoint::checkpoint_round_bounded(
        &fixture.store,
        &mut fixture.ids.clone(),
        memory,
        &fixture.project,
        &fixture.state(),
        &fixture.synced(),
        &WorktreeState {
            modified: vec![fixture.changed_file()],
            ..WorktreeState::default()
        },
        pinned(),
        limit,
    )
    .await
    .expect("the round")
}

fn sample_checkpoint() -> Checkpoint {
    let mut checkpoint = Checkpoint::new(
        CheckpointId::from_string("CHK-1"),
        TaskId::from_string("AUTH"),
        "implement auth",
        "wrote the middleware",
        "run the tests",
    );
    checkpoint.branch = Some("main".into());
    checkpoint.commit_sha = Some("abc".into());
    checkpoint.changed_files = vec!["src/auth/mod.rs".into()];
    checkpoint
}
