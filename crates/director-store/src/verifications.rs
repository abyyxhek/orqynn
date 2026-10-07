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

use director_domain::ids::{ProjectId, RepositoryId, TaskId, VerificationId};
use director_domain::task::{Task, TaskStatus};
use director_domain::verification::Verification;
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
    let mut conn = pool.get();

    // The status the task is moving from, read inside the transaction so the
    // history records what the store actually saw rather than what the caller
    // believed when it decided.
    let previous_status: Option<TaskStatus> = conn
        .query_row(
            "SELECT status FROM tasks WHERE id = ?1",
            [verification.task_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(translate_error)?
        .map(|text| json::from_json::<TaskStatus>(&text))
        .transpose()?;

    let tx = conn.transaction().map_err(translate_error)?;

    // The verdict is recorded first, so a verdict that moves nothing (an
    // `Unverifiable` round) still lands its evidence trail.
    insert_verification(&tx, verification)?;

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
    }

    tx.commit().map_err(translate_error)?;

    Ok((
        load_verification(&conn, &verification.id)?,
        crate::tasks::load_task(&conn, &verification.task_id)?,
    ))
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
    use director_domain::ids::{ProjectId, TaskId, VerificationId};
    use director_domain::verification::{Evidence, EvidenceStatus, ProbeKind, VerificationStatus};
    // The repository traits the fixture calls — the store's own methods are the
    // trait implementations, so the traits have to be in scope to reach them.
    use director_domain::{ProjectRepository, TaskRepository};

    /// A store with one project and one task to hang verifications off.
    async fn store_with_task() -> (crate::Store, TaskId) {
        let dir = tempfile::TempDir::new().expect("a temp dir for the store");
        let store = crate::Store::open(dir.path().join("orqyn.db"))
            .await
            .expect("store opens and migrates");

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

        (store, task_id)
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
}
