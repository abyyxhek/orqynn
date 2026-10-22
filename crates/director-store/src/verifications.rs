//! [`VerificationRepository`] over SQLite.
//!
//! The behavior that matters here is that a verification is *history*, not
//! state. There is no update path and no delete path in this module: re-judging
//! a task writes a new row with a new id, and the rows already written are left
//! exactly as they were. That is what makes "Orqyn judged this task three times
//! — it failed twice on a broken suite and passed on the third attempt" an
//! ordinary query against [`verifications_for_task`] rather than a forensic
//! reconstruction from logs.
//!
//! The evidence column is read and written as one unit with its verification,
//! the way a task's expected outputs are. Nothing ever selects "all verifications
//! whose evidence contains a failure", so a join table would buy nothing and
//! cost a consistency boundary.

use async_trait::async_trait;
use rusqlite::OptionalExtension;

use director_domain::ids::{Id, ProjectId, RepositoryId, TaskId, VerificationId};
use director_domain::task::{Task, TaskStatus};
use director_domain::verification::{Evidence, Verification, VerificationStatus};
use director_domain::{StoreError, VerificationRepository};

use crate::connection::{translate_error, PooledConn};
use crate::json;

/// The store-facing implementation of [`VerificationRepository`].
#[derive(Clone)]
pub struct SqliteVerificationRepository {
    pool: crate::connection::ConnectionPool,
}

impl SqliteVerificationRepository {
    pub(crate) fn new(pool: crate::connection::ConnectionPool) -> Self {
        SqliteVerificationRepository { pool }
    }

    fn conn(&self) -> PooledConn {
        self.pool.get()
    }
}

#[async_trait]
impl director_domain::Store for SqliteVerificationRepository {
    type Error = StoreError;
}

#[async_trait]
impl VerificationRepository for SqliteVerificationRepository {
    async fn create_verification(
        &self,
        verification: &Verification,
    ) -> Result<Verification, StoreError> {
        let verification = verification.clone();
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(translate_error)?;

        insert_verification(&tx, &verification)?;

        tx.commit().map_err(translate_error)?;
        load_verification(&conn, &verification.id)
    }

    async fn get_verification(&self, id: &VerificationId) -> Result<Verification, StoreError> {
        let conn = self.conn();
        load_verification(&conn, id)
    }

    async fn latest_verification(&self, task: &TaskId) -> Result<Option<Verification>, StoreError> {
        let conn = self.conn();
        // Newest first by the id surrogate, which is monotonic with creation —
        // the same ordering `verifications_for_task` uses.
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {VERIFICATION_COLUMNS}
                 FROM verifications WHERE task_id = ?1 ORDER BY created_at DESC, id DESC LIMIT 1"
            ))
            .map_err(translate_error)?;
        let mut rows = stmt
            .query_map([task.as_str()], row_to_verification)
            .map_err(translate_error)?;
        match rows.next() {
            Some(row) => Ok(Some(row.map_err(translate_error)?)),
            None => Ok(None),
        }
    }

    async fn verifications_for_task(&self, task: &TaskId) -> Result<Vec<Verification>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {VERIFICATION_COLUMNS}
                 FROM verifications WHERE task_id = ?1 ORDER BY created_at DESC, id DESC"
            ))
            .map_err(translate_error)?;
        let rows = stmt
            .query_map([task.as_str()], row_to_verification)
            .map_err(translate_error)?;
        let mut all = Vec::new();
        for row in rows {
            all.push(row.map_err(translate_error)?);
        }
        Ok(all)
    }
}

/// Insert one verification row. Called inside the caller's transaction so a
/// verification never lands without the evidence it describes — and so
/// [`crate::Store::apply_verification`] can write the verification, the task's
/// status move, and the transition history in one transaction, making a verdict
/// that moved a task without recording why impossible.
pub(crate) fn insert_verification(
    tx: &rusqlite::Transaction<'_>,
    verification: &Verification,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO verifications (id, task_id, project_id, repository_id, status, evidence,
                                    head_commit, state_version, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            verification.id.as_str(),
            verification.task_id.as_str(),
            verification.project_id.as_str(),
            verification
                .repository_id
                .as_ref()
                .map(RepositoryId::as_str),
            json::to_json(&verification.status)?,
            json::to_json(&verification.evidence)?,
            verification.head_commit.as_deref(),
            verification.state_version as i64,
            json::timestamp(verification.created_at),
        ],
    )
    .map_err(translate_error)?;
    Ok(())
}

/// Load one verification by id, or [`StoreError::NotFound`].
pub(crate) fn load_verification(
    conn: &PooledConn,
    id: &VerificationId,
) -> Result<Verification, StoreError> {
    conn.query_row(
        &format!("SELECT {VERIFICATION_COLUMNS} FROM verifications WHERE id = ?1"),
        [id.as_str()],
        row_to_verification,
    )
    .map_err(translate_error)
}

/// The columns every read of a verification selects, in order. Kept in one place
/// so the insert, the by-id load, and the history scans cannot drift apart.
const VERIFICATION_COLUMNS: &str = "id, task_id, project_id, repository_id, status, evidence,
        head_commit, state_version, created_at";

/// Map a row into a [`Verification`]. A malformed column is an error rather than
/// a silent default — see [`crate::json`] for why that matters here: an
/// evidence column that silently deserialized to `[]` would present a judgment
/// Orqyn reached as one it reached on no evidence at all.
fn row_to_verification(row: &rusqlite::Row<'_>) -> rusqlite::Result<Verification> {
    let repository_id: Option<String> = row.get(3)?;
    let status: String = row.get(4)?;
    let evidence: String = row.get(5)?;
    let head_commit: Option<String> = row.get(6)?;
    let created_at: String = row.get(8)?;

    // `Verification::new` takes the fields in its own order; the column order
    // above is the store's, and the two are deliberately not required to agree.
    let mut verification = Verification::new(
        VerificationId::from_string(row.get::<_, String>(0)?),
        TaskId::from_string(row.get::<_, String>(1)?),
        ProjectId::from_string(row.get::<_, String>(2)?),
        repository_id.map(RepositoryId::from_string),
        // The status is reconstructed rather than trusted: a column holding
        // something the model no longer recognizes is a serialization failure,
        // not a verdict.
        json::from_json(&status).map_err(|err| sqlite_conv_failure(4, err))?,
        json::from_json(&evidence).map_err(|err| sqlite_conv_failure(5, err))?,
        head_commit,
        json::parse_timestamp(&created_at).map_err(|err| sqlite_conv_failure(8, err))?,
    );
    verification.state_version = row.get::<_, i64>(7)? as u64;
    Ok(verification)
}

/// Record a verification and apply its verdict to the task, atomically: the
/// verification row, the task's status move, and the transition history row that
/// records the move — one transaction, all or nothing.
///
/// This is the operation the verification engine uses to land a judgment. The
/// three writes cannot be left in separate calls, because the window between
/// them is exactly the state a crash would strand. Between the verification row
/// and the status move it would be a task that reads `done` with no recorded
/// judgment — and `done` is the status the whole model exists to make
/// unreachable without independent evidence, so a `done` with no verification is
/// not a degraded record, it is a broken invariant. Between the status move and
/// the history row it would be a move the transition trail does not describe,
/// which is the same problem [`crate::tasks`] already solved by writing both in
/// one transaction.
///
/// `expected_task_version` is the optimistic-concurrency guard, and it is the
/// version of the task the round *read* before it gathered its evidence — not a
/// version the caller re-reads just before calling. A verdict is a statement
/// about the work the task held at a specific version; if the task has moved on
/// since the round looked at it, the evidence no longer describes the work, and
/// the update matches no row and becomes a [`StoreError::StateVersionConflict`]
/// rather than a silent overwrite. This is the same guard `update_task`,
/// `cancel_task`, and `update_agent` carry, and for the same reason: two writers
/// who both read version *n* cannot both write *n+1*. A verdict that moves
/// nothing (an `Unverifiable` round) does not touch the task row, so it cannot
/// conflict — the evidence trail lands regardless, which is what makes "Orqyn
/// looked, and could not yet tell" history worth keeping.
///
/// The task's destination is *derived from the verdict*, not taken from the
/// caller's task: [`VerificationStatus::task_status`] is the single place the
/// mapping lives, so the store and the loop's step cannot disagree about what a
/// `Passed` verdict means. An `Unverifiable` verdict still writes the
/// verification row — "Orqyn looked, and could not yet tell" is history worth
/// keeping — and moves nothing, which is also what makes it safe to survey the
/// task again next tick.
///
/// Returns the verification and the task as they now stand, read back from the
/// store rather than echoed from the input.
pub async fn apply_verification(
    pool: &crate::connection::ConnectionPool,
    verification: &Verification,
    expected_task_version: u64,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(Verification, Task), StoreError> {
    apply_verification_with_actions(pool, verification, &[], expected_task_version, now).await
}

/// [`apply_verification`], with the actions a round observed written in the same
/// transaction as the verdict they justify.
///
/// This is the whole point of the operation existing. The verdict commits here and
/// the actions used to commit afterwards, in transactions of their own, which
/// left a window in which a process could die holding a durable verdict and no
/// record of what it had run — a briefing that claimed a decision it could not
/// account for. Inserting them inside this transaction makes the pair
/// all-or-nothing: a reader sees the verdict and its observations, or neither.
///
/// Three properties this relies on, all already true of the surrounding code:
///
/// - **The transaction is the same one.** The actions go through
///   [`crate::actions::insert_action`] on *this* [`rusqlite::Transaction`], so a
///   failure anywhere in the block rolls back the verification row, the task's
///   move and the history row along with them.
/// - **Locking is unchanged.** This transaction is deferred, so it takes the
///   write lock at its first write — which is the verification insert, before the
///   actions. Concurrent writers therefore serialise there, and no other writer
///   can observe or interleave with a half-written pair.
/// - **The state-version check still decides.** The optimistic precondition on
///   `tasks.state_version` is untouched, and a stale round is refused before its
///   actions are attempted.
///
/// Actions are ignored for a round that moves no task status. An `Unverifiable`
/// round stays in the survey set and is run again next tick, so recording its
/// attempts would append one copy of the same attempt per tick. The gate is
/// enforced here as well as by the caller, because "a round that will be repeated
/// records nothing" is a property of what is durable and belongs at the boundary
/// rather than in a convention.
pub async fn apply_verification_with_actions(
    pool: &crate::connection::ConnectionPool,
    verification: &Verification,
    actions: &[director_domain::action::Action],
    expected_task_version: u64,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(Verification, Task), StoreError> {
    apply_verification_resolved(pool, verification, actions, expected_task_version, now).await
}

/// The shared body of [`apply_verification`] and its actions-bearing sibling, so
/// the two cannot drift on the parts that must be identical.
async fn apply_verification_resolved(
    pool: &crate::connection::ConnectionPool,
    verification: &Verification,
    actions: &[director_domain::action::Action],
    expected_task_version: u64,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(Verification, Task), StoreError> {
    let mut conn = pool.get();

    // The status the task is moving from, read outside the transaction — under
    // a deferred transaction the read snapshot is as good as any, because this
    // path has nothing to allocate: the caller named the id, and the write
    // lock is taken at the first write below either way.
    let previous_status = read_task_status(&conn, &verification.task_id)?;

    let tx = conn.transaction().map_err(translate_error)?;
    land_verification(
        &tx,
        previous_status,
        verification,
        actions,
        expected_task_version,
        now,
    )?;
    tx.commit().map_err(translate_error)?;

    Ok((
        load_verification(&conn, &verification.id)?,
        crate::tasks::load_task(&conn, &verification.task_id)?,
    ))
}

/// [`apply_verification_with_actions`], with the verification id allocated
/// inside the write transaction rather than supplied by the caller.
///
/// The id cannot honestly be chosen by the caller *and* be free of a race: the
/// ordinal is a count of the task's judgments, and any count read outside the
/// write lock is a guess. Two rounds reading the same count build the same
/// `VER-{task}-{n}`, and the loser is refused by the primary key on
/// `verifications.id` — a legitimate judgment lost to a naming collision,
/// nothing about which was wrong. This is the same reservation problem
/// [`crate::blockers::create_next`] and [`crate::assignments::assign_and_start_next`]
/// solved, and the fix is the same shape: the count and the insert happen on
/// one `IMMEDIATE` transaction, which reserves the write lock *before* it
/// reads, so a competing writer cannot interleave between them. The count the
/// ordinal comes from therefore includes every judgment committed before this
/// one and none committed after it.
///
/// The ordinal is monotonic over the task's whole history, because
/// verifications are never deleted — a re-judgment's id can never collide with
/// the row it deliberately does not replace — and it survives a process
/// restart, because nothing about it depends on a process-local counter.
///
/// `actions_for` builds the round's observations from the verification this
/// transaction *allocated*, not from one the caller invented. An action id
/// embeds its verification's id (`ACT-{verification}-{n}`), so the caller
/// cannot build them before the call: it does not yet know the id. Taking a
/// builder rather than a pre-built slice is what lets the allocation and the
/// keying stay on the same side of the transaction boundary.
///
/// This is additive. [`apply_verification_with_actions`] keeps its signature
/// and its caller-chosen-id behaviour, and the domain trait is unchanged:
/// allocation is a store concern, so it is offered here rather than as a new
/// obligation on every implementation of the trait.
#[allow(clippy::too_many_arguments)] // the verdict's fields, one per — the shape `Verification::new` takes
pub async fn apply_verification_with_actions_next<F>(
    pool: &crate::connection::ConnectionPool,
    task_id: &TaskId,
    project_id: &ProjectId,
    repository_id: Option<RepositoryId>,
    status: VerificationStatus,
    evidence: Vec<Evidence>,
    head_commit: Option<String>,
    actions_for: F,
    expected_task_version: u64,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(Verification, Task), StoreError>
where
    F: FnOnce(&Verification) -> Vec<director_domain::action::Action>,
{
    let mut conn = pool.get();

    // `IMMEDIATE`, not the deferred default, and this is the whole point. A
    // deferred transaction takes the write lock at its first *write*, so its
    // earlier reads come from a snapshot taken before any competing writer
    // committed — exactly the window the count below exists to close. The
    // reserved write lock is held from here until commit, so the count and the
    // insert a few lines below cannot be separated by another writer.
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(translate_error)?;

    // The status the task is moving from, read on the transaction itself: this
    // path has to see the state the write lock serializes, not a snapshot from
    // before it.
    let previous_status = read_task_status(&tx, task_id)?;

    let verification = Verification::new(
        next_verification_id(&tx, task_id)?,
        task_id.clone(),
        project_id.clone(),
        repository_id,
        status,
        evidence,
        head_commit,
        now,
    );

    // The observations, keyed on the id this transaction just allocated. Built
    // after the allocation and inside it, so the pair cannot be keyed wrongly.
    let actions = actions_for(&verification);
    land_verification(
        &tx,
        previous_status,
        &verification,
        &actions,
        expected_task_version,
        now,
    )?;
    tx.commit().map_err(translate_error)?;

    Ok((
        load_verification(&conn, &verification.id)?,
        crate::tasks::load_task(&conn, &verification.task_id)?,
    ))
}

/// Write one verification and everything its verdict implies, on `tx`.
///
/// The shared body of the caller-chosen-id and store-allocated-id paths, so the
/// two cannot drift on the parts that must be identical: the verdict row, the
/// task's compare-and-swap move, the transition that records the move, and the
/// observations the verdict ran. A failure anywhere in the block rolls back all
/// of them, which is what the caller's transaction is for.
fn land_verification(
    tx: &rusqlite::Transaction<'_>,
    previous_status: Option<TaskStatus>,
    verification: &Verification,
    actions: &[director_domain::action::Action],
    expected_task_version: u64,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), StoreError> {
    // The verdict is recorded first, so a verdict that moves nothing (an
    // `Unverifiable` round) still lands its evidence trail.
    insert_verification(tx, verification)?;

    // The task move the verdict implies. `None` means the round reached no
    // verdict and the task is left exactly as it was — which is what makes a
    // round that could not gather evidence safe to run again next tick.
    if let Some(destination) = verification.status.task_status() {
        // The version the round read when it gathered its evidence. A zero-row
        // update means the task moved after that read, so the evidence no longer
        // describes the work and the whole transaction is refused — including
        // the verification row above, which is what keeps a stale round from
        // landing a judgment against a task somebody else already moved.
        let rows = tx
            .execute(
                "UPDATE tasks
                    SET status = ?2, state_version = state_version + 1, updated_at = ?3
                  WHERE id = ?1 AND state_version = ?4",
                rusqlite::params![
                    verification.task_id.as_str(),
                    json::to_json(&destination)?,
                    json::timestamp(now),
                    expected_task_version as i64,
                ],
            )
            .map_err(translate_error)?;
        crate::connection::row_count_to_outcome(
            rows,
            "task",
            verification.task_id.as_str(),
            expected_task_version,
        )?;

        if let Some(from) = previous_status {
            if from != destination {
                tx.execute(
                    "INSERT INTO task_status_history
                        (task_id, from_status, to_status, occurred_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![
                        verification.task_id.as_str(),
                        json::to_json(&from)?,
                        json::to_json(&destination)?,
                        json::timestamp(now),
                    ],
                )
                .map_err(translate_error)?;
            }
        }

        // The observations behind this verdict, on the same transaction and the
        // same lock as the verdict itself. Only for a round that actually decided
        // something: an `Unverifiable` round is re-surveyed next tick, so its
        // attempts are deliberately not recorded.
        if verification.status.task_status().is_some() {
            for action in actions {
                crate::actions::insert_action(tx, action)?;
            }
        }
    }

    Ok(())
}

/// The task's status as the store sees it, or `None` when the task is absent.
///
/// Read on `conn`, which is either a plain connection (the caller-chosen path)
/// or the transaction itself (the allocation path, where the read has to share
/// the write lock rather than precede it).
fn read_task_status(
    conn: &rusqlite::Connection,
    task_id: &TaskId,
) -> Result<Option<TaskStatus>, StoreError> {
    conn.query_row(
        "SELECT status FROM tasks WHERE id = ?1",
        [task_id.as_str()],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(translate_error)?
    .map(|text| json::from_json::<TaskStatus>(&text))
    .transpose()
}

/// The next verification id for `task`, counted inside the caller's transaction.
///
/// `VER-{task}-{n}`, where `n` counts the judgments already committed for the
/// task — every row, because verifications are append-only history, which is
/// what makes the ordinal monotonic and keeps a re-judgment's id from colliding
/// with the row it is deliberately not replacing. The count is meaningful only
/// because it is read under the same write lock as the insert that consumes it:
/// on its own, outside a transaction, the same count is what used to give two
/// concurrent rounds the same name.
fn next_verification_id(
    tx: &rusqlite::Transaction<'_>,
    task_id: &TaskId,
) -> Result<VerificationId, StoreError> {
    let prior: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM verifications WHERE task_id = ?1",
            [task_id.as_str()],
            |row| row.get(0),
        )
        .map_err(translate_error)?;
    Ok(VerificationId::from_string(format!(
        "{}-{task_id}-{}",
        VerificationId::PREFIX,
        prior + 1
    )))
}

fn sqlite_conv_failure(index: usize, err: StoreError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, err.into())
}

#[cfg(test)]
mod tests {
    //! These tests exercise the repository against a real SQLite database, not a
    //! mock. The reason is the same one that holds across this crate: a query
    //! that looks right in Rust and behaves wrong in SQL is a class of bug only
    //! a real database catches, and the JSON encoding of the enum columns is
    //! where that class lives — see [`crate::json`] and the partial-index
    //! comment in migration 0002.

    use super::*;
    use std::sync::{Arc, Barrier};

    use director_domain::action::Action;
    use director_domain::ids::{ActionId, ProjectId, TaskId, VerificationId};
    use director_domain::verification::{Evidence, EvidenceStatus, ProbeKind, VerificationStatus};
    // The repository traits the fixture calls — the store's own methods are the
    // trait implementations, so the traits have to be in scope to reach them.
    use director_domain::{ActionRepository, ProjectRepository, TaskRepository};

    /// A pinned instant, so the history a test reads back is the history it wrote.
    fn pinned() -> chrono::DateTime<chrono::Utc> {
        chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 9, 9, 8, 0, 0).unwrap()
    }

    /// Drive one future to completion on the calling thread, for tests that
    /// need real OS threads around the store.
    ///
    /// The store's calls contain no yielding `.await`, so a `tokio::join!` would
    /// run them start-to-finish on one thread and prove nothing about a race.
    /// Each thread gets its own runtime instead, the way the store's own
    /// concurrency tests do it.
    fn run<T, F>(future: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime")
            .block_on(future)
    }

    /// Seed one project and one `verification_pending` task into `store`, the
    /// state every verification test hangs its judgments off.
    async fn seed(store: &crate::Store) -> TaskId {
        let project = ProjectId::from_string("PROJ-1");
        store
            .projects()
            .create_project(&director_domain::project::Project::new(
                project.clone(),
                project.as_str(),
                "/nowhere",
            ))
            .await
            .expect("project created");

        let task_id = TaskId::from_string("AUTH-42");
        let mut task =
            director_domain::task::Task::for_project(project, task_id.clone(), "Auth", "login");
        task.status = director_domain::task::TaskStatus::VerificationPending;
        store
            .tasks()
            .create_task(&task)
            .await
            .expect("task created");

        task_id
    }

    /// A store with one project and one task to hang verifications off.
    async fn store_with_task() -> (crate::Store, TaskId) {
        let dir = tempfile::TempDir::new().expect("a temp dir for the store");
        let store = crate::Store::open(dir.path().join("orqyn.db"))
            .await
            .expect("store opens and migrates");

        let task_id = seed(&store).await;
        (store, task_id)
    }

    /// A database path holding one project and one `verification_pending` task,
    /// for tests that need more than one store looking at the same file — the
    /// shape concurrency actually has.
    ///
    /// The directory is returned so it outlives the test; the file is the
    /// shared state, and every store that opens it sees the same rows.
    async fn shared_db_with_task() -> (std::path::PathBuf, tempfile::TempDir, TaskId) {
        let dir = tempfile::TempDir::new().expect("a temp dir");
        let path = dir.path().join("orqyn.db");
        let store = crate::Store::open(&path)
            .await
            .expect("store opens and migrates");
        let task_id = seed(&store).await;
        drop(store);
        (path, dir, task_id)
    }

    /// One piece of decisive evidence, the judgment a passing round reached.
    fn passing_evidence() -> Vec<Evidence> {
        vec![Evidence {
            kind: ProbeKind::Command,
            criterion: "login works".into(),
            status: EvidenceStatus::Passed,
            detail: "exit code 0".into(),
        }]
    }

    /// One verification with a single piece of decisive evidence.
    fn passing_verification(id: &str, task: &TaskId) -> Verification {
        Verification::new(
            VerificationId::from_string(id),
            task.clone(),
            ProjectId::from_string("PROJ-1"),
            None,
            VerificationStatus::Passed,
            vec![Evidence {
                kind: ProbeKind::Command,
                criterion: "login works".into(),
                status: EvidenceStatus::Passed,
                detail: "exit code 0".into(),
            }],
            None,
            chrono::Utc::now(),
        )
    }

    #[tokio::test]
    async fn a_verification_round_trips_through_the_store() {
        let (store, task) = store_with_task().await;
        let verification = passing_verification("VER-1", &task);

        let stored = store
            .verifications()
            .create_verification(&verification)
            .await
            .expect("verification written");

        // The evidence trail is what the caller reads back, and it has to
        // survive the JSON column intact — an empty evidence array here would
        // mean a verdict with no recorded basis.
        assert_eq!(stored, verification);
        assert_eq!(stored.evidence.len(), 1);
        assert_eq!(stored.evidence[0].criterion, "login works");
        assert_eq!(stored.evidence[0].status, EvidenceStatus::Passed);

        let loaded = store
            .verifications()
            .get_verification(&verification.id)
            .await
            .expect("verification loaded");
        assert_eq!(loaded, verification);
    }

    #[tokio::test]
    async fn advisory_evidence_survives_the_round_trip() {
        // An Observed status is the one a caller most needs to see preserved:
        // it is the diff probe's "decides nothing" report, and silently
        // dropping or converting it would either manufacture a verdict or hide
        // the one signal a human wanted.
        let (store, task) = store_with_task().await;
        let verification = Verification::new(
            VerificationId::from_string("VER-1"),
            task.clone(),
            ProjectId::from_string("PROJ-1"),
            Some(RepositoryId::from_string("REPO-1")),
            VerificationStatus::Passed,
            vec![
                Evidence {
                    kind: ProbeKind::TestSuite,
                    criterion: "the suite passes".into(),
                    status: EvidenceStatus::Passed,
                    detail: "14 passed, 0 failed".into(),
                },
                Evidence {
                    kind: ProbeKind::Diff,
                    criterion: "the work touched the declared scope".into(),
                    status: EvidenceStatus::Observed,
                    detail: "no changes within src/auth/".into(),
                },
                Evidence {
                    kind: ProbeKind::File,
                    criterion: "the report exists".into(),
                    status: EvidenceStatus::Unverifiable,
                    detail: "path check unavailable: no working directory".into(),
                },
            ],
            Some("abc123".into()),
            chrono::Utc::now(),
        );

        let stored = store
            .verifications()
            .create_verification(&verification)
            .await
            .expect("verification written");

        assert_eq!(stored.evidence.len(), 3);
        assert_eq!(stored.evidence[1].status, EvidenceStatus::Observed);
        assert_eq!(stored.evidence[2].status, EvidenceStatus::Unverifiable);
        assert_eq!(
            stored.repository_id.as_ref().map(RepositoryId::as_str),
            Some("REPO-1")
        );
        assert_eq!(stored.head_commit.as_deref(), Some("abc123"));
    }

    #[tokio::test]
    async fn a_duplicate_verification_id_is_refused() {
        // Two verifications sharing an id is a judgment history that has lost a
        // row: whichever write won, the reader cannot tell which judgment came
        // first.
        let (store, task) = store_with_task().await;
        store
            .verifications()
            .create_verification(&passing_verification("VER-1", &task))
            .await
            .expect("first verification written");

        let err = store
            .verifications()
            .create_verification(&passing_verification("VER-1", &task))
            .await
            .expect_err("a duplicate id is refused");
        assert!(matches!(err, StoreError::ConstraintViolation(_)));
    }

    #[tokio::test]
    async fn a_verification_for_an_unknown_task_is_refused() {
        // A verification hanging off a task that does not exist is a judgment
        // about nothing, and a dangling reference is how "what was judged"
        // stops being answerable.
        let (store, _task) = store_with_task().await;
        let err = store
            .verifications()
            .create_verification(&passing_verification(
                "VER-9",
                &TaskId::from_string("NOPE-1"),
            ))
            .await
            .expect_err("an unknown task is refused");
        assert!(matches!(err, StoreError::ConstraintViolation(_)));
    }

    #[tokio::test]
    async fn an_unknown_verification_id_is_not_found() {
        let (store, _task) = store_with_task().await;
        let err = store
            .verifications()
            .get_verification(&VerificationId::from_string("VER-404"))
            .await
            .expect_err("a missing id is not found");
        assert!(matches!(err, StoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn a_task_with_no_verifications_has_no_latest() {
        let (store, task) = store_with_task().await;
        assert!(store
            .verifications()
            .latest_verification(&task)
            .await
            .expect("an empty history is not an error")
            .is_none());
        assert!(store
            .verifications()
            .verifications_for_task(&task)
            .await
            .expect("an empty history is not an error")
            .is_empty());
    }

    #[tokio::test]
    async fn verifications_accumulate_as_history_newest_first() {
        // The point of the entity: a task judged three times carries all three
        // judgments, in the order a reader wants them.
        let (store, task) = store_with_task().await;

        let first = passing_verification("VER-1", &task);
        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut second = passing_verification("VER-2", &task);
        second.status = VerificationStatus::Failed;
        std::thread::sleep(std::time::Duration::from_millis(10));
        let third = passing_verification("VER-3", &task);

        store
            .verifications()
            .create_verification(&first)
            .await
            .expect("first written");
        store
            .verifications()
            .create_verification(&second)
            .await
            .expect("second written");
        store
            .verifications()
            .create_verification(&third)
            .await
            .expect("third written");

        let history = store
            .verifications()
            .verifications_for_task(&task)
            .await
            .expect("history loaded");
        assert_eq!(history.len(), 3);
        // Newest first.
        assert_eq!(history[0].id, third.id);
        assert_eq!(history[1].id, second.id);
        assert_eq!(history[2].id, first.id);
        // The middle judgment's failure survived — history is not overwritten.
        assert_eq!(history[1].status, VerificationStatus::Failed);

        let latest = store
            .verifications()
            .latest_verification(&task)
            .await
            .expect("latest loaded")
            .expect("there is one");
        assert_eq!(latest.id, third.id);
    }

    #[tokio::test]
    async fn each_tasks_history_is_independent() {
        let (store, task) = store_with_task().await;
        let other = TaskId::from_string("OTHER-1");
        let project = ProjectId::from_string("PROJ-1");
        let mut other_task =
            director_domain::task::Task::for_project(project, other.clone(), "Other", "thing");
        other_task.status = director_domain::task::TaskStatus::VerificationPending;
        store
            .tasks()
            .create_task(&other_task)
            .await
            .expect("other task created");

        store
            .verifications()
            .create_verification(&passing_verification("VER-1", &task))
            .await
            .expect("first task verified");

        let mut for_other = passing_verification("VER-2", &other);
        for_other.task_id = other.clone();
        store
            .verifications()
            .create_verification(&for_other)
            .await
            .expect("other task verified");

        assert_eq!(
            store
                .verifications()
                .verifications_for_task(&task)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .verifications()
                .verifications_for_task(&other)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    /// The column list is used by every read path; if it drifts from the insert,
    /// a load would select a different shape than the write produced. The row
    /// mapper reads by index, so the count is the invariant that matters — nine
    /// columns in the same order the insert writes them.
    #[test]
    fn the_column_list_names_every_column_once() {
        let columns: Vec<String> = VERIFICATION_COLUMNS
            .split_whitespace()
            .map(|c| c.trim_end_matches(',').to_string())
            .collect();
        let expected = [
            "id",
            "task_id",
            "project_id",
            "repository_id",
            "status",
            "evidence",
            "head_commit",
            "state_version",
            "created_at",
        ];
        assert_eq!(columns, expected);
        // The insert writes the same nine, in the same order.
        assert_eq!(expected.len(), 9);
    }

    #[test]
    fn malformed_evidence_is_a_hard_error_not_an_empty_trail() {
        // `row_to_verification` reconstructs the status and the evidence from
        // their columns rather than trusting them. A column holding something
        // the model no longer recognizes is a [`StoreError::Serialization`],
        // never a silent default — an evidence array that deserialized to `[]`
        // would present a judgment Orqyn reached as one it reached on no
        // evidence at all, and a status that fell back to a default would
        // present a verdict the round never reached.
        let err = json::from_json::<Vec<Evidence>>("this is not an evidence array")
            .expect_err("garbage is refused");
        assert!(matches!(err, StoreError::Serialization(_)));

        let err = json::from_json::<VerificationStatus>("\"speculative\"")
            .expect_err("an unknown status is refused");
        assert!(matches!(err, StoreError::Serialization(_)));

        // The round trip the store actually performs still works, so a failure
        // here means the column drifted rather than that the reader is broken.
        let evidence = vec![Evidence {
            kind: ProbeKind::Command,
            criterion: "login works".into(),
            status: EvidenceStatus::Failed,
            detail: "exit 3".into(),
        }];
        let text = json::to_json(&evidence).expect("serializes");
        assert_eq!(
            json::from_json::<Vec<Evidence>>(&text).expect("deserializes"),
            evidence
        );
        assert_eq!(
            json::from_json::<VerificationStatus>(
                &json::to_json(&VerificationStatus::Passed).unwrap()
            )
            .expect("a known status round trips"),
            VerificationStatus::Passed
        );
    }

    // ---- The allocation race, and what closes it -----------------------------

    /// Two concurrent verifications of one task cannot allocate the same id.
    ///
    /// This is the race the verification engine used to lose: both rounds read
    /// the count of the task's judgments *before* the write, both built
    /// `VER-AUTH-42-1`, and the second was refused by the primary key — a
    /// judgment Orqyn tried to keep, lost to nothing but its name. Allocation
    /// now happens inside an `IMMEDIATE` transaction, so the count that names
    /// the ordinal and the insert that consumes it share one reserved write
    /// lock.
    ///
    /// The rendezvous is what makes this deterministic rather than a bet on
    /// timing. Thread A's round holds its action builder — which runs *after*
    /// the id is allocated and *before* anything is written — parked on a
    /// channel, and thread B does not start its own round until A is parked
    /// there. B is therefore forced inside the exact window the old engine
    /// raced in: A has read the count, A has not committed, and B starts from
    /// scratch. Given that interleaving:
    ///
    /// - under the reserved write lock, B's transaction cannot *begin* until A
    ///   commits, so B's count includes A's row and B names `VER-AUTH-42-2`;
    /// - without the lock, B reads the same stale count A did, and the pair
    ///   resolves by error — a primary-key collision on the name, or the
    ///   `database is locked` a stale deferred snapshot earns at commit. Which
    ///   of the two is incidental; that one of the two rounds *dies* is the
    ///   failure this test exists to catch.
    ///
    /// Both rounds are `Unverifiable` — a verdict that moves nothing — so both
    /// are expected to commit; a refusal would be a judgment lost to a naming
    /// collision, not an acceptable outcome.
    #[test]
    fn concurrent_verifications_of_one_task_never_share_an_id() {
        let (path, _dir, task) = run(shared_db_with_task());
        let project = ProjectId::from_string("PROJ-1");

        // B's store is opened before A starts, so no connection setup contends
        // with the round being watched.
        let store_b = run(async {
            crate::Store::open(&path)
                .await
                .expect("B's store opens on the shared file")
        });

        let (a_announced, a_watcher) = std::sync::mpsc::channel::<()>();
        let (b_go, b_watcher) = std::sync::mpsc::channel::<()>();

        let path_a = path.clone();
        let task_a = task.clone();
        let project_a = project.clone();
        let a = std::thread::spawn(move || {
            run(async move {
                let store = crate::Store::open(&path_a)
                    .await
                    .expect("A's store opens on the shared file");
                store
                    .apply_verification_with_actions_next(
                        &task_a,
                        &project_a,
                        None,
                        VerificationStatus::Unverifiable,
                        vec![],
                        None,
                        |_verification| {
                            // Mid-transaction: the id is allocated, nothing is
                            // written yet. Say so, and wait for B's reply — this
                            // is the held-open window between the count and the
                            // insert.
                            a_announced
                                .send(())
                                .expect("B is listening for A's announcement");
                            b_watcher
                                .recv_timeout(std::time::Duration::from_secs(10))
                                .expect("B releases A within a deadline");
                            Vec::new()
                        },
                        1,
                        pinned(),
                    )
                    .await
                    .map(|(verification, _)| verification.id)
            })
        });

        // B waits until A's round is mid-flight, then starts its own.
        a_watcher
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("A announces that its round has named an id");
        b_go.send(()).expect("A is parked, waiting for B's go");

        let b_id = run(async move {
            store_b
                .apply_verification_with_actions_next(
                    &task,
                    &project,
                    None,
                    VerificationStatus::Unverifiable,
                    vec![],
                    None,
                    |_| Vec::new(),
                    1,
                    pinned(),
                )
                .await
                .map(|(verification, _)| verification.id)
        })
        .expect("B's round lands — its count ran after A's commit");

        let a_id = a
            .join()
            .expect("A finishes")
            .expect("A's round lands — its name was unique when it was taken");

        assert_eq!(
            a_id.to_string(),
            "VER-AUTH-42-1",
            "A's round named the first judgment, and kept it"
        );
        assert_eq!(
            b_id.to_string(),
            "VER-AUTH-42-2",
            "B's count saw A's row before it named its own — the ordinal is \
             taken under the write lock, not guessed before it"
        );
    }

    /// The failure mode, shown on the path that still has it.
    ///
    /// The caller-chosen-id behaviour of [`apply_verification_with_actions`] is
    /// unchanged, and deliberately — callers that have an id keep it. This test
    /// exists so the difference is visible: two rounds that read the count
    /// *outside* the write window (the barrier stands in for the round's work)
    /// both name their judgment `VER-AUTH-42-1`, and one of them dies on the
    /// primary key for no reason but the stale count.
    ///
    /// Everything about the losing verdict was sound; only its name was taken.
    /// [`apply_verification_with_actions_next`] is the same judgment without
    /// the naming step, and the previous test is what it buys.
    #[test]
    fn a_count_read_outside_the_write_still_collides_when_the_caller_names_the_id() {
        let (path, _dir, task) = run(shared_db_with_task());
        let gate = Arc::new(Barrier::new(2));

        let outcomes = std::thread::scope(|scope| {
            let threads = [0, 1].map(|_| {
                let path = path.clone();
                let task = task.clone();
                let path_for_count = path.clone();
                let task_for_count = task.clone();
                let gate = Arc::clone(&gate);
                scope.spawn(move || {
                    // The count is read *before* the barrier, as the old
                    // `next_verification_id` read it before the round's work:
                    // both threads see zero, because neither has written yet.
                    let id = run(async move {
                        let store = crate::Store::open(&path_for_count)
                            .await
                            .expect("the store opens on the shared file");
                        let already = store
                            .verifications()
                            .verifications_for_task(&task_for_count)
                            .await
                            .expect("the history reads")
                            .len();
                        VerificationId::from_string(format!(
                            "{}-{task_for_count}-{}",
                            VerificationId::PREFIX,
                            already + 1
                        ))
                    });

                    gate.wait();

                    run(async move {
                        let store = crate::Store::open(&path)
                            .await
                            .expect("the store opens on the shared file");
                        let verification = Verification::new(
                            id,
                            task,
                            ProjectId::from_string("PROJ-1"),
                            None,
                            VerificationStatus::Unverifiable,
                            vec![],
                            None,
                            pinned(),
                        );
                        store
                            .verifications()
                            .create_verification(&verification)
                            .await
                            .map(|_| ())
                    })
                })
            });
            threads
                .into_iter()
                .map(|handle| handle.join().expect("the thread finishes"))
                .collect::<Vec<_>>()
        });

        let landed = outcomes.iter().filter(|o| o.is_ok()).count();
        let refused = outcomes
            .iter()
            .filter_map(|o| o.as_ref().err())
            .collect::<Vec<_>>();
        assert_eq!(landed, 1, "one round won the name: {outcomes:?}");
        assert_eq!(refused.len(), 1, "and exactly one lost it");
        assert!(
            matches!(refused[0], StoreError::ConstraintViolation(_)),
            "the refusal is the primary key, got: {}",
            refused[0]
        );

        // The one judgment that did land is on record, exactly once.
        let store = run(async move {
            crate::Store::open(&path)
                .await
                .expect("the store reopens to check what landed")
        });
        let history = run(async move {
            store
                .verifications()
                .verifications_for_task(&task)
                .await
                .expect("the history reads")
        });
        assert_eq!(
            history
                .into_iter()
                .map(|v| v.id.to_string())
                .collect::<Vec<_>>(),
            vec!["VER-AUTH-42-1".to_string()],
            "one row for one name — the other round's judgment is simply absent"
        );
    }

    /// A store-allocated verdict lands everything it implies at once.
    ///
    /// The verification row, the task's move, the transition that records the
    /// move, and the observations behind the verdict are one transaction — and
    /// the observations are keyed on the id the store allocated, which the
    /// caller could not have known before the call.
    #[tokio::test]
    async fn a_store_allocated_verdict_lands_its_row_move_history_and_actions_at_once() {
        let (store, task) = store_with_task().await;
        let project = ProjectId::from_string("PROJ-1");
        let version = store
            .tasks()
            .get_task(&task)
            .await
            .expect("the task reads")
            .state_version;

        let (stored, moved) = store
            .apply_verification_with_actions_next(
                &task,
                &project,
                None,
                VerificationStatus::Passed,
                passing_evidence(),
                None,
                |verification| {
                    vec![Action::new(
                        ActionId::from_string(format!("ACT-{}-0", verification.id)),
                        director_domain::action::ActionKind::Command,
                        "ran the check",
                    )
                    .for_task(verification.task_id.clone())]
                },
                version,
                pinned(),
            )
            .await
            .expect("the verdict lands");

        // The store named the judgment, continuing the sequence on record.
        assert_eq!(stored.id.as_str(), "VER-AUTH-42-1");
        assert_eq!(stored.created_at, pinned());

        // The task moved, and its version advanced by exactly that one write.
        assert_eq!(moved.status, director_domain::task::TaskStatus::Done);
        assert_eq!(
            moved.state_version,
            version + 1,
            "the version advanced once, for the one transaction"
        );
        assert_eq!(
            store.tasks().get_task(&task).await.expect("reads").status,
            director_domain::task::TaskStatus::Done
        );

        // The transition the move implies, recorded by the same transaction.
        let history = store
            .tasks()
            .task_history(&task)
            .await
            .expect("the history reads");
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0].from,
            director_domain::task::TaskStatus::VerificationPending
        );
        assert_eq!(history[0].to, director_domain::task::TaskStatus::Done);

        // The observations, keyed on the allocated id — the caller built them
        // from the record the store handed back, and nothing else.
        let actions = store
            .actions()
            .actions_for_task(&task)
            .await
            .expect("the actions read");
        assert_eq!(
            actions
                .into_iter()
                .map(|action| action.id.to_string())
                .collect::<Vec<_>>(),
            vec!["ACT-VER-AUTH-42-1-0".to_string()]
        );

        // And the judgment itself reads back through the ordinary path.
        assert_eq!(
            store
                .verifications()
                .get_verification(&stored.id)
                .await
                .expect("the verification reads")
                .status,
            VerificationStatus::Passed
        );
    }

    /// A round whose task has moved is refused, and leaves nothing behind.
    ///
    /// The compare-and-swap is the part of the operation that has to *not*
    /// retry: the round's evidence describes a task that no longer exists in
    /// that state, so a refusal is the correct answer, not a transient one.
    /// The transaction that refuses it rolls back the verification row it had
    /// already written, so no judgment lands against a task it does not
    /// describe — and no history, and no orphan actions.
    #[tokio::test]
    async fn a_stale_version_refuses_a_store_allocated_verdict_and_writes_nothing() {
        let (store, task) = store_with_task().await;
        let project = ProjectId::from_string("PROJ-1");
        let version = store
            .tasks()
            .get_task(&task)
            .await
            .expect("the task reads")
            .state_version;

        let err = store
            .apply_verification_with_actions_next(
                &task,
                &project,
                None,
                VerificationStatus::Passed,
                passing_evidence(),
                None,
                |verification| {
                    vec![Action::new(
                        ActionId::from_string(format!("ACT-{}-0", verification.id)),
                        director_domain::action::ActionKind::Command,
                        "ran the check",
                    )
                    .for_task(verification.task_id.clone())]
                },
                // A version the task has never held: the round is stale by
                // construction, deterministically, rather than by timing.
                version + 99,
                pinned(),
            )
            .await
            .expect_err("a stale round is refused");
        assert!(
            matches!(err, StoreError::StateVersionConflict { .. }),
            "the refusal is the optimistic precondition, got: {err}"
        );

        assert!(
            store
                .verifications()
                .verifications_for_task(&task)
                .await
                .expect("the history reads")
                .is_empty(),
            "not even the evidence trail landed — the judgment describes nothing real"
        );
        assert!(
            store
                .actions()
                .actions_for_task(&task)
                .await
                .expect("the actions read")
                .is_empty(),
            "no orphan actions for a verdict that never landed"
        );
        assert!(
            store
                .tasks()
                .task_history(&task)
                .await
                .expect("the history reads")
                .is_empty(),
            "no transition, because nothing moved"
        );
        let after = store.tasks().get_task(&task).await.expect("reads");
        assert_eq!(
            after.status,
            director_domain::task::TaskStatus::VerificationPending
        );
        assert_eq!(
            after.state_version, version,
            "the task is exactly as it was"
        );
    }

    /// An unrelated constraint failure surfaces as itself and takes the verdict
    /// with it.
    ///
    /// The observation below names a task that does not exist, so its insert
    /// violates the foreign key on `task_actions.task_id`. That is a
    /// [`StoreError::ConstraintViolation`] — the same class a duplicate id is
    /// under this store's error vocabulary — and this is the discrimination
    /// that matters: the failure must be reported as the foreign key it is and
    /// roll the round back, never be mistaken for a naming collision that
    /// regenerating an id could fix. Under the old engine-side retry, which
    /// matched on the error's *message text*, this would have been the case
    /// where a text match quietly decided which failures survive.
    #[tokio::test]
    async fn an_unrelated_constraint_failure_surfaces_and_takes_the_verdict_with_it() {
        let (store, task) = store_with_task().await;
        let project = ProjectId::from_string("PROJ-1");
        let version = store
            .tasks()
            .get_task(&task)
            .await
            .expect("the task reads")
            .state_version;

        let err = store
            .apply_verification_with_actions_next(
                &task,
                &project,
                None,
                VerificationStatus::Passed,
                passing_evidence(),
                None,
                |verification| {
                    vec![Action::new(
                        ActionId::from_string(format!("ACT-{}-0", verification.id)),
                        director_domain::action::ActionKind::Command,
                        "an observation against a task that is not there",
                    )
                    .for_task(TaskId::from_string("NO-SUCH-TASK"))]
                },
                version,
                pinned(),
            )
            .await
            .expect_err("the round is refused");
        assert!(
            matches!(err, StoreError::ConstraintViolation(_)),
            "the foreign key failure is reported, not swallowed and not reclassified: {err}"
        );

        assert!(
            store
                .verifications()
                .verifications_for_task(&task)
                .await
                .expect("the history reads")
                .is_empty(),
            "the verdict rolled back with the observation that could not be written"
        );
        assert_eq!(
            store.tasks().get_task(&task).await.expect("reads").status,
            director_domain::task::TaskStatus::VerificationPending,
            "the task did not move on a round that landed nothing"
        );
    }

    /// A verdict for a task that does not exist is refused by the foreign key.
    ///
    /// The allocated-id path has no caller-supplied state to lean on, so this
    /// is the check that nothing about it relaxed the referential rule the
    /// caller-chosen path already carries.
    #[tokio::test]
    async fn a_store_allocated_verdict_for_an_unknown_task_is_refused() {
        let (store, _task) = store_with_task().await;
        let err = store
            .apply_verification_with_actions_next(
                &TaskId::from_string("NOPE-1"),
                &ProjectId::from_string("PROJ-1"),
                None,
                VerificationStatus::Unverifiable,
                vec![],
                None,
                |_| Vec::new(),
                1,
                pinned(),
            )
            .await
            .expect_err("an unknown task is refused");
        assert!(
            matches!(err, StoreError::ConstraintViolation(_)),
            "a dangling reference is refused, got: {err}"
        );
    }

    /// The allocated id continues the sequence the store already holds.
    ///
    /// The ordinal counts every committed judgment for the task, not the ones
    /// this process wrote, which is what makes a re-judgment after a restart a
    /// new row rather than a collision — the property the Phase 11 recovery
    /// work pinned for caller-chosen ids, now held by the allocating path.
    #[tokio::test]
    async fn the_allocated_id_continues_the_sequence_the_store_holds() {
        let (store, task) = store_with_task().await;
        let project = ProjectId::from_string("PROJ-1");
        let version = store
            .tasks()
            .get_task(&task)
            .await
            .expect("the task reads")
            .state_version;

        // One judgment already on record — written by the caller-chosen path,
        // as a restart would have written it.
        store
            .verifications()
            .create_verification(&passing_verification("VER-AUTH-42-1", &task))
            .await
            .expect("the first judgment is written");

        let (second, _) = store
            .apply_verification_with_actions_next(
                &task,
                &project,
                None,
                VerificationStatus::Failed,
                vec![Evidence {
                    kind: ProbeKind::Command,
                    criterion: "login works".into(),
                    status: EvidenceStatus::Failed,
                    detail: "exit 3".into(),
                }],
                None,
                |_| Vec::new(),
                version,
                pinned(),
            )
            .await
            .expect("the re-judgment lands");

        assert_eq!(
            second.id.as_str(),
            "VER-AUTH-42-2",
            "the ordinal continues the committed history, not any process-local count"
        );
        assert_eq!(
            store
                .verifications()
                .verifications_for_task(&task)
                .await
                .expect("the history reads")
                .len(),
            2,
            "both judgments are on record — history accumulates, it does not overwrite"
        );
    }
}
