//! [`CheckpointRepository`] over SQLite.
//!
//! The behavior that matters here is the supersession rule: creating a
//! checkpoint marks the previous current one for the same task `superseded`,
//! and that row is *not deleted*. A task's recovery history accumulates rather
//! than being overwritten, which is what makes "what did the last agent think
//! the state was" answerable after a second agent has already moved on.
//!
//! Both the marking and the insert are one transaction, so a task is never left
//! with two current checkpoints — the partial unique index in the schema is the
//! second line of defense against a concurrent double-write.
//!
//! ## Supersession is version-gated
//!
//! The marking is not unconditional. `create_checkpoint` takes the
//! `state_version` of the current checkpoint the caller read before writing,
//! and the supersession matches on it. A writer whose read is stale — another
//! round landed a checkpoint between its read and its write — supersedes
//! nothing and gets a [`StoreError::StateVersionConflict`], so a slower writer
//! can never bury a checkpoint a faster one already wrote. The check and the
//! write share one transaction, so a refused write leaves the lineage
//! untouched: no row is superseded, no row is inserted.
//!
//! ### The race the gate alone does not catch
//!
//! The transaction is deferred, so the gate `SELECT` is what fixes the
//! transaction's read snapshot. A second writer that reads *before* the first
//! commits therefore passes the gate honestly — both saw version N — and the
//! conflict only surfaces when it tries to write against a snapshot the first
//! writer has already moved past. SQLite refuses that write with `SQLITE_BUSY`
//! rather than silently applying it to a stale view.
//!
//! [`translate_error`] on its own reports that as a constraint violation,
//! which is the wrong vocabulary: it tells the caller the store is broken when
//! the fact is precisely the opposite — the store held the invariant, and the
//! caller's read is stale. [`translate_supersession_error`] maps it to the same
//! [`StoreError::StateVersionConflict`] the gate returns, so a caller that
//! re-reads and retries sees one condition rather than two, and the stored
//! version it is told about is the one it needs to retry against.
//!
//! ## Supersession and insertion are one transaction
//!
//! The marking and the insert commit together, so a task is never left with
//! two current checkpoints and never left with none. If the insert fails — a
//! duplicate checkpoint id, most commonly — the transaction is dropped and the
//! supersession is undone with it, leaving the sitting checkpoint current at
//! its original version. The partial unique index in the schema is the second
//! line of defense: it refuses a second current row even if a future code path
//! forgets to supersede first.

use async_trait::async_trait;
use rusqlite::OptionalExtension;

use director_domain::checkpoint::{Checkpoint, CheckpointStatus};
use director_domain::ids::{AgentId, CheckpointId, SessionId, TaskId};
use director_domain::StoreError;

use crate::connection::{translate_error, PooledConn};
use crate::json;

/// The store-facing implementation of [`CheckpointRepository`].
#[derive(Clone)]
pub struct SqliteCheckpointRepository {
    pool: crate::connection::ConnectionPool,
}

impl SqliteCheckpointRepository {
    pub(crate) fn new(pool: crate::connection::ConnectionPool) -> Self {
        SqliteCheckpointRepository { pool }
    }

    fn conn(&self) -> PooledConn {
        self.pool.get()
    }
}

#[async_trait]
impl director_domain::Store for SqliteCheckpointRepository {
    type Error = StoreError;
}

#[async_trait]
impl director_domain::CheckpointRepository for SqliteCheckpointRepository {
    async fn create_checkpoint(
        &self,
        checkpoint: &Checkpoint,
        expected_current_version: Option<u64>,
    ) -> Result<Checkpoint, StoreError> {
        let checkpoint = checkpoint.clone();
        let mut conn = self.conn();

        let tx = conn.transaction().map_err(translate_error)?;

        // The version gate, read inside the transaction so it cannot be beaten
        // by a concurrent write between the check and the supersession. The
        // stored status is JSON-encoded ("current", quotes and all), so the
        // predicate compares against the JSON form — a bare 'current' here
        // would match nothing and every write would look conflicting.
        let stored_version: Option<i64> = tx
            .query_row(
                "SELECT state_version FROM checkpoints
                  WHERE task_id = ?1 AND status = ?2",
                rusqlite::params![
                    checkpoint.task_id.as_str(),
                    json::to_json(&CheckpointStatus::Current)?,
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(translate_error)?;

        // The caller's precondition must match the world as it now stands. A
        // stored row when the caller expected none, a different version when the
        // caller read one, and a missing row when the caller read a version are
        // all the same thing: somebody else moved the checkpoint forward, and
        // superseding it would bury the newer state. The stored version is
        // reported precisely, because unlike a plain update this path already
        // has it in hand.
        let current_version = stored_version.map(|v| v as u64);
        if current_version != expected_current_version {
            return Err(StoreError::StateVersionConflict {
                entity: "checkpoint",
                id: checkpoint.task_id.to_string(),
                stored: current_version.unwrap_or(expected_current_version.unwrap_or(0) + 1),
                caller: expected_current_version.unwrap_or(0),
            });
        }

        // The store assigns the new checkpoint's `state_version` from the
        // precondition rather than trusting the caller's field, because the
        // version is a *lineage* position: each checkpoint a task accumulates
        // is one higher than the one it supersedes, so the current row's version
        // advances and a stale reader is distinguishable from a fresh one. A
        // caller that constructed `Checkpoint::new` — which starts at 1 — would
        // otherwise leave every checkpoint in the lineage at 1, and the gate
        // above could never fire.
        let lineage_version = expected_current_version.map(|v| v + 1).unwrap_or(1);

        // The previous current checkpoint for this task is superseded, not
        // deleted. Same transaction as the insert, so the two never diverge: if
        // the insert below fails, the transaction is dropped and this marking is
        // undone with it, leaving the sitting checkpoint current. The version is
        // part of the predicate, which is what makes a stale writer a no-op
        // rather than a clobber. Status is compared as JSON, because that is how
        // it is written: a bare `'current'` in the predicate never matches a
        // column holding `"current"`.
        let superseded = tx
            .execute(
                "UPDATE checkpoints SET status = ?2, state_version = state_version + 1
                  WHERE task_id = ?1 AND status = ?3 AND state_version = ?4",
                rusqlite::params![
                    checkpoint.task_id.as_str(),
                    json::to_json(&CheckpointStatus::Superseded)?,
                    json::to_json(&CheckpointStatus::Current)?,
                    stored_version,
                ],
            )
            .map_err(|err| {
                translate_supersession_error(err, &checkpoint.task_id, stored_version)
            })?;

        // The gate and the supersession must agree. The gate found a current row
        // at `stored_version`, so the update above had exactly one row to match.
        // Zero rows means the row moved between the gate and the write — the
        // race the gate cannot catch, because both writers read the same version
        // before either wrote. SQLite normally refuses such a write with
        // SQLITE_BUSY, which the translation above already turns into a conflict;
        // this assertion is the second line of defense, so a future change that
        // quieted that error could not silently leave a task with two current
        // checkpoints. It costs one comparison on the write path.
        if stored_version.is_some() && superseded == 0 {
            return Err(StoreError::StateVersionConflict {
                entity: "checkpoint",
                id: checkpoint.task_id.to_string(),
                stored: current_version.unwrap_or(expected_current_version.unwrap_or(0) + 1),
                caller: expected_current_version.unwrap_or(0),
            });
        }

        tx.execute(
            "INSERT INTO checkpoints (id, task_id, objective, progress, progress_fraction,
                                      branch, commit_sha, changed_files, test_results,
                                      current_blocker, important_decisions, current_assumptions,
                                      next_action, context_version, project_state_version,
                                      task_state_version, status, created_at, created_by_agent,
                                      created_by_session, state_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17, ?18, ?19, ?20, ?21)",
            rusqlite::params![
                checkpoint.id.as_str(),
                checkpoint.task_id.as_str(),
                checkpoint.objective,
                checkpoint.progress,
                checkpoint.progress_fraction,
                checkpoint.branch,
                checkpoint.commit_sha,
                json::to_json(&checkpoint.changed_files)?,
                json::to_json_or_null(checkpoint.test_results.as_ref())?,
                checkpoint.current_blocker,
                json::to_json(&checkpoint.important_decisions)?,
                json::to_json(&checkpoint.current_assumptions)?,
                checkpoint.next_action,
                checkpoint.context_version as i64,
                checkpoint.project_state_version.map(|v| v as i64),
                checkpoint.task_state_version.map(|v| v as i64),
                json::to_json(&checkpoint.status)?,
                json::timestamp(checkpoint.created_at),
                checkpoint.created_by_agent.as_ref().map(|a| a.as_str()),
                checkpoint.created_by_session.as_ref().map(|s| s.as_str()),
                lineage_version as i64,
            ],
        )
        .map_err(translate_error)?;

        tx.commit().map_err(|err| {
            translate_supersession_error(err, &checkpoint.task_id, stored_version)
        })?;
        load_checkpoint(&conn, &checkpoint.id)
    }

    async fn get_checkpoint(&self, id: &CheckpointId) -> Result<Checkpoint, StoreError> {
        let conn = self.conn();
        load_checkpoint(&conn, id)
    }

    async fn latest_checkpoint(&self, task: &TaskId) -> Result<Option<Checkpoint>, StoreError> {
        let conn = self.conn();
        conn.query_row(
            "SELECT id, task_id, objective, progress, progress_fraction, branch, commit_sha,
                    changed_files, test_results, current_blocker, important_decisions,
                    current_assumptions, next_action, context_version, project_state_version,
                    task_state_version, status, created_at, created_by_agent,
                    created_by_session, state_version
             FROM checkpoints WHERE task_id = ?1 AND status = ?2",
            rusqlite::params![task.as_str(), json::to_json(&CheckpointStatus::Current)?,],
            row_to_checkpoint,
        )
        .optional()
        .map_err(translate_error)
    }

    async fn checkpoints_for_task(&self, task: &TaskId) -> Result<Vec<Checkpoint>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, task_id, objective, progress, progress_fraction, branch,
                        commit_sha, changed_files, test_results, current_blocker,
                        important_decisions, current_assumptions, next_action,
                        context_version, project_state_version, task_state_version, status,
                        created_at, created_by_agent, created_by_session, state_version
                 FROM checkpoints WHERE task_id = ?1 ORDER BY created_at DESC",
            )
            .map_err(translate_error)?;
        let rows = stmt
            .query_map([task.as_str()], row_to_checkpoint)
            .map_err(translate_error)?;
        let mut all = Vec::new();
        for row in rows {
            all.push(row.map_err(translate_error)?);
        }
        Ok(all)
    }
}

/// Translate an error from the supersession half of `create_checkpoint`.
///
/// [`translate_error`] alone misreads the race that matters here. The gate
/// `SELECT` fixes the transaction's snapshot, so a writer that read a current
/// checkpoint *before* another writer superseded it passes the gate honestly —
/// both saw the same version — and SQLite refuses its write against the stale
/// snapshot with `SQLITE_BUSY`. Reported as a generic constraint violation,
/// that looks like a broken store to a caller matching on
/// [`StoreError::StateVersionConflict`], and the recovery path — re-read and
/// retry — is invisible.
///
/// It is the same fact the gate reports: another writer moved the lineage, so
/// this write changes nothing. Translating it the same way keeps the caller's
/// retry logic single-branch, and the `stored` version the caller is handed is
/// the one it must present next time, since it is the version that beat it.
fn translate_supersession_error(
    err: rusqlite::Error,
    task: &TaskId,
    stored_version: Option<i64>,
) -> StoreError {
    match err {
        // SQLITE_BUSY, most often as SQLITE_BUSY_TIMEOUT once the busy_timeout
        // has exhausted its retries. Both spell the same thing to this code: the
        // snapshot the transaction holds is no longer the one it can write to.
        rusqlite::Error::SqliteFailure(ref failure, _)
            if failure.code == rusqlite::ErrorCode::DatabaseBusy =>
        {
            StoreError::StateVersionConflict {
                entity: "checkpoint",
                id: task.to_string(),
                // The version that beat this writer. The gate already read it;
                // this is that value, not a guess, so a retry against it lands
                // rather than conflicting a second time.
                stored: stored_version.map(|v| v as u64).unwrap_or(1),
                // The writer's own read, which the caller also knows. The
                // conflict is only meaningful against it: it is the version this
                // write was keyed on.
                caller: stored_version.map(|v| v as u64).unwrap_or(0),
            }
        }
        // Anything else is not a race: a duplicate id, a broken serialization, a
        // genuine storage fault. Those keep the vocabulary `translate_error`
        // already gives them.
        other => translate_error(other),
    }
}

/// Load one checkpoint by id, or [`StoreError::NotFound`].
pub(crate) fn load_checkpoint(
    conn: &PooledConn,
    id: &CheckpointId,
) -> Result<Checkpoint, StoreError> {
    conn.query_row(
        "SELECT id, task_id, objective, progress, progress_fraction, branch, commit_sha,
                changed_files, test_results, current_blocker, important_decisions,
                current_assumptions, next_action, context_version, project_state_version,
                task_state_version, status, created_at, created_by_agent,
                created_by_session, state_version
         FROM checkpoints WHERE id = ?1",
        [id.as_str()],
        row_to_checkpoint,
    )
    .map_err(translate_error)
}

fn row_to_checkpoint(row: &rusqlite::Row<'_>) -> rusqlite::Result<Checkpoint> {
    let branch: Option<String> = row.get(5)?;
    let commit_sha: Option<String> = row.get(6)?;
    let test_results: Option<String> = row.get(8)?;
    let current_blocker: Option<String> = row.get(9)?;
    let created_by_agent: Option<String> = row.get(18)?;
    let created_by_session: Option<String> = row.get(19)?;

    let mut cp = Checkpoint::new(
        CheckpointId::from_string(row.get::<_, String>(0)?),
        TaskId::from_string(row.get::<_, String>(1)?),
        String::new(),
        String::new(),
        String::new(),
    );
    cp.objective = row.get(2)?;
    cp.progress = row.get(3)?;
    cp.progress_fraction = row.get(4)?;
    cp.branch = branch;
    cp.commit_sha = commit_sha;
    cp.changed_files =
        json::from_json(&row.get::<_, String>(7)?).map_err(|err| sqlite_conv_failure(7, err))?;
    cp.test_results = json::from_json_or_none(test_results.as_deref())
        .map_err(|err| sqlite_conv_failure(8, err))?;
    cp.current_blocker = current_blocker;
    cp.important_decisions =
        json::from_json(&row.get::<_, String>(10)?).map_err(|err| sqlite_conv_failure(10, err))?;
    cp.current_assumptions =
        json::from_json(&row.get::<_, String>(11)?).map_err(|err| sqlite_conv_failure(11, err))?;
    cp.next_action = row.get(12)?;
    cp.context_version = row.get::<_, i64>(13)? as u32;
    cp.project_state_version = row.get::<_, Option<i64>>(14)?.map(|v| v as u64);
    cp.task_state_version = row.get::<_, Option<i64>>(15)?.map(|v| v as u64);
    cp.status =
        json::from_json(&row.get::<_, String>(16)?).map_err(|err| sqlite_conv_failure(16, err))?;
    cp.created_at = json::parse_timestamp(&row.get::<_, String>(17)?)
        .map_err(|err| sqlite_conv_failure(17, err))?;
    cp.created_by_agent = created_by_agent.map(AgentId::from_string);
    cp.created_by_session = created_by_session.map(SessionId::from_string);
    cp.state_version = row.get::<_, i64>(20)? as u64;
    Ok(cp)
}

fn sqlite_conv_failure(index: usize, err: StoreError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, err.into())
}
