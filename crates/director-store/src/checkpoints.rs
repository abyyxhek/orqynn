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
    async fn create_checkpoint(&self, checkpoint: &Checkpoint) -> Result<Checkpoint, StoreError> {
        let checkpoint = checkpoint.clone();
        let mut conn = self.conn();

        let tx = conn.transaction().map_err(translate_error)?;

        // The previous current checkpoint for this task is superseded, not
        // deleted. Same transaction as the insert, so the two never diverge.
        // Status is compared as JSON, because that is how it is written: a bare
        // `'current'` in the predicate never matches a column holding
        // `"current"`.
        tx.execute(
            "UPDATE checkpoints SET status = ?2, state_version = state_version + 1
              WHERE task_id = ?1 AND status = ?3",
            rusqlite::params![
                checkpoint.task_id.as_str(),
                json::to_json(&CheckpointStatus::Superseded)?,
                json::to_json(&CheckpointStatus::Current)?,
            ],
        )
        .map_err(translate_error)?;

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
                checkpoint.state_version as i64,
            ],
        )
        .map_err(translate_error)?;

        tx.commit().map_err(translate_error)?;
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
