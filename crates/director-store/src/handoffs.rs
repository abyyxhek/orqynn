//! Handoffs: the auditable record that one agent transferred continuation
//! responsibility for a task to another.
//!
//! Deliberately narrow. This repository records attribution and nothing else:
//!
//!   * It never assigns a task, never changes a task's status, and never
//!     completes anything. Execution authority is the assignment lease's job and a
//!     handoff does not touch it.
//!   * It never reads or writes verification evidence. The continuation narrative
//!     it stores is what an outgoing agent *said*, which is context and never
//!     proof that the work was done.
//!
//! The narrative is an immutable snapshot written once at creation, together with
//! the source checkpoint and task version it was derived from. Those two columns
//! are what make staleness askable: a reader compares them against the task as it
//! stands now rather than trusting prose. A snapshot with no recorded derivation
//! point reads as stale, which is the conservative answer — never "current
//! because there was nothing to contradict it".
//!
//! Acceptance is claim-once and decided by a single conditional `UPDATE`, so
//! concurrent attempts resolve to exactly one winner by construction rather than
//! by a check-then-write that could interleave.

use director_domain::handoff::{Handoff, HandoffState};
use director_domain::ids::{AgentId, HandoffId, SessionId, TaskId};

use async_trait::async_trait;
use director_domain::StoreError;
use rusqlite::OptionalExtension;

use crate::connection::{translate_error, PooledConn};
use crate::json;

/// The columns every handoff query selects, in order. One place, so the SELECTs
/// and the row mapper cannot drift apart.
const HANDOFF_COLUMNS: &str = "id, task_id, project_id, from_agent, from_session, to_agent, \
     state, summary, open_questions, next_steps, files_touched, source_checkpoint, \
     source_task_version, created_at, accepted_at, accepted_by_session, cause";

/// The store-facing implementation of [`director_domain::HandoffRepository`].
#[derive(Clone)]
pub struct SqliteHandoffRepository {
    pool: crate::connection::ConnectionPool,
}

impl SqliteHandoffRepository {
    pub(crate) fn new(pool: crate::connection::ConnectionPool) -> Self {
        SqliteHandoffRepository { pool }
    }

    fn conn(&self) -> PooledConn {
        self.pool.get()
    }
}

#[async_trait]
impl director_domain::Store for SqliteHandoffRepository {
    type Error = StoreError;
}

#[async_trait]
impl director_domain::HandoffRepository for SqliteHandoffRepository {
    async fn open_handoff(&self, handoff: &Handoff) -> Result<Handoff, StoreError> {
        let handoff = handoff.clone();
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(translate_error)?;

        // The task must exist. The foreign key would catch it, but saying so
        // directly turns a storage error into a reason a caller can act on, and
        // the whole row is rolled back with it.
        let task_exists: bool = tx
            .query_row(
                "SELECT 1 FROM tasks WHERE id = ?1",
                [handoff.task_id.as_str()],
                |_| Ok(()),
            )
            .optional()
            .map_err(translate_error)?
            .is_some();
        if !task_exists {
            return Err(StoreError::NotFound(format!(
                "task {} does not exist, so nothing was recorded for it",
                handoff.task_id.as_str()
            )));
        }

        // The source agent must exist too: a handoff attributed to an agent
        // Orqyn has never registered is not an attribution.
        let agent_exists: bool = tx
            .query_row(
                "SELECT 1 FROM agents WHERE id = ?1",
                [handoff.from_agent.as_str()],
                |_| Ok(()),
            )
            .optional()
            .map_err(translate_error)?
            .is_some();
        if !agent_exists {
            return Err(StoreError::NotFound(format!(
                "agent {} does not exist, so a handoff cannot be attributed to it",
                handoff.from_agent.as_str()
            )));
        }

        insert_handoff(&tx, &handoff)?;
        tx.commit().map_err(translate_error)?;
        drop(conn);

        let conn = self.conn();
        load_handoff(&conn, &handoff.id)
    }

    async fn get_handoff(&self, id: &HandoffId) -> Result<Handoff, StoreError> {
        let conn = self.conn();
        load_handoff(&conn, id)
    }

    async fn accept_handoff(
        &self,
        id: &HandoffId,
        acceptor: &AgentId,
        session: &SessionId,
        accepted_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<Handoff, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(translate_error)?;

        // Identity and liveness are checked inside the write transaction, so a
        // handoff cannot be accepted between the check and the update.
        let current: Option<(String, String)> = tx
            .query_row(
                "SELECT to_agent, state FROM handoffs WHERE id = ?1",
                [id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(translate_error)?;
        let Some((to_agent, state)) = current else {
            return Err(StoreError::NotFound(format!(
                "handoff {} does not exist",
                id.as_str()
            )));
        };
        if to_agent != acceptor.as_str() {
            return Err(StoreError::ConstraintViolation(format!(
                "handoff {} is addressed to {to_agent}, so {acceptor} cannot accept it",
                id.as_str()
            )));
        }
        if state != open_state() {
            return Err(StoreError::ConstraintViolation(format!(
                "handoff {} is {state}, so it cannot be accepted",
                id.as_str()
            )));
        }

        // The decision is the row count. `WHERE state = 'open'` means the second
        // of two concurrent attempts updates nothing, whatever it read a moment
        // earlier, so there is exactly one winner without a lock being taken on
        // the way in.
        let updated = tx
            .execute(
                "UPDATE handoffs
                    SET state = ?2, accepted_at = ?3, accepted_by_session = ?4
                  WHERE id = ?1 AND state = ?5",
                rusqlite::params![
                    id.as_str(),
                    json::to_json(&HandoffState::Accepted)?,
                    json::timestamp(accepted_at),
                    session.as_str(),
                    open_state(),
                ],
            )
            .map_err(translate_error)?;
        if updated != 1 {
            return Err(StoreError::ConstraintViolation(format!(
                "handoff {} was accepted by another caller first",
                id.as_str()
            )));
        }

        tx.commit().map_err(translate_error)?;
        drop(conn);

        let conn = self.conn();
        load_handoff(&conn, id)
    }

    async fn open_handoffs(&self) -> Result<Vec<Handoff>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {HANDOFF_COLUMNS} FROM handoffs WHERE state = ?1 ORDER BY created_at DESC"
            ))
            .map_err(translate_error)?;
        let rows = stmt
            .query_map([open_state()], row_to_handoff)
            .map_err(translate_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(translate_error)
    }

    async fn handoffs_for_task(&self, task: &TaskId) -> Result<Vec<Handoff>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {HANDOFF_COLUMNS} FROM handoffs
                  WHERE task_id = ?1 ORDER BY created_at DESC"
            ))
            .map_err(translate_error)?;
        let rows = stmt
            .query_map([task.as_str()], row_to_handoff)
            .map_err(translate_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(translate_error)
    }
}

/// The stored spelling of `HandoffState::Open`, used in the conditional UPDATE so
/// the predicate and the comparison cannot drift apart.
fn open_state() -> String {
    json::to_json(&HandoffState::Open).unwrap_or_else(|_| "\"Open\"".to_string())
}

/// This repository also satisfies [`director_domain::providers::HandoffProvider`].
///
/// Provided so Orqynn's own handoff substrate can be handed wherever a
/// `HandoffProvider` is expected. The mapping is deliberately thin: the provider
/// is a second name for the same four operations, and it grants nothing the
/// repository does not.
#[async_trait]
impl director_domain::providers::Provider for SqliteHandoffRepository {
    type Error = StoreError;
}

#[async_trait]
impl director_domain::providers::HandoffProvider for SqliteHandoffRepository {
    async fn open_handoff(&self, handoff: Handoff) -> Result<Handoff, StoreError> {
        director_domain::HandoffRepository::open_handoff(self, &handoff).await
    }

    async fn get_handoff(&self, id: &HandoffId) -> Result<Option<Handoff>, StoreError> {
        match director_domain::HandoffRepository::get_handoff(self, id).await {
            Ok(handoff) => Ok(Some(handoff)),
            Err(StoreError::NotFound(_)) => Ok(None),
            Err(other) => Err(other),
        }
    }

    async fn accept_handoff(
        &self,
        id: &HandoffId,
        acceptor: &AgentId,
        session: &SessionId,
    ) -> Result<Handoff, StoreError> {
        director_domain::HandoffRepository::accept_handoff(
            self,
            id,
            acceptor,
            session,
            chrono::Utc::now(),
        )
        .await
    }

    async fn open_handoffs(&self) -> Result<Vec<Handoff>, StoreError> {
        director_domain::HandoffRepository::open_handoffs(self).await
    }
}

/// Write one handoff row.
fn insert_handoff(tx: &rusqlite::Transaction<'_>, handoff: &Handoff) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO handoffs
            (id, task_id, project_id, from_agent, from_session, to_agent, state,
             summary, open_questions, next_steps, files_touched,
             source_checkpoint, source_task_version,
             created_at, accepted_at, accepted_by_session, cause)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
        rusqlite::params![
            handoff.id.as_str(),
            handoff.task_id.as_str(),
            handoff.project_id.as_ref().map(|id| id.as_str()),
            handoff.from_agent.as_str(),
            handoff.from_session.as_ref().map(|id| id.as_str()),
            handoff.to_agent.as_str(),
            json::to_json(&handoff.state)?,
            handoff.summary,
            json::to_json(&handoff.open_questions)?,
            json::to_json(&handoff.next_steps)?,
            json::to_json(&handoff.files_touched)?,
            handoff.source_checkpoint.as_ref().map(|id| id.as_str()),
            handoff.source_task_version.map(|v| v as i64),
            json::timestamp(handoff.created_at),
            handoff.accepted_at.map(crate::json::timestamp),
            handoff.accepted_by_session.as_ref().map(|id| id.as_str()),
            // A handoff written through the repository carries its cause when the
            // caller established one. `None` is stored as NULL - "unknown", never
            // a default - which is also what a pre-`0006` row holds.
            handoff
                .cause
                .map(|cause| json::to_json(&cause))
                .transpose()?,
        ],
    )
    .map_err(translate_error)?;
    Ok(())
}

/// Load one handoff by id, or [`StoreError::NotFound`].
fn load_handoff(conn: &PooledConn, id: &HandoffId) -> Result<Handoff, StoreError> {
    conn.query_row(
        &format!("SELECT {HANDOFF_COLUMNS} FROM handoffs WHERE id = ?1"),
        [id.as_str()],
        row_to_handoff,
    )
    .map_err(translate_error)
}

/// Map a row into a [`Handoff`]. A bad column is an error, not a default: a
/// handoff whose narrative cannot be read back is not a handoff that quietly
/// lost some of its context.
fn row_to_handoff(row: &rusqlite::Row<'_>) -> rusqlite::Result<Handoff> {
    let project_id: Option<String> = row.get(2)?;
    let from_session: Option<String> = row.get(4)?;
    let state: String = row.get(6)?;
    let open_questions: String = row.get(8)?;
    let next_steps: String = row.get(9)?;
    let files_touched: String = row.get(10)?;
    let source_checkpoint: Option<String> = row.get(11)?;
    let source_task_version: Option<i64> = row.get(12)?;
    let accepted_at: Option<String> = row.get(14)?;
    let accepted_by_session: Option<String> = row.get(15)?;
    let cause: Option<String> = row.get(16)?;

    // A bad column is an error, not a default: a handoff whose narrative cannot be
    // read back is not a handoff that quietly lost some of its context.

    // Read with explicit types: the Id constructors are generic-looking at the
    // call site, and inference cannot pick `String` for a bare `row.get(..)?`.
    let id: String = row.get(0)?;
    let task_id: String = row.get(1)?;
    let from_agent: String = row.get(3)?;
    let to_agent: String = row.get(5)?;

    Ok(Handoff {
        id: director_domain::ids::HandoffId::from_string(id),
        task_id: TaskId::from_string(task_id),
        project_id: project_id.map(director_domain::ids::ProjectId::from_string),
        from_agent: AgentId::from_string(from_agent),
        from_session: from_session.map(SessionId::from_string),
        to_agent: AgentId::from_string(to_agent),
        state: json::from_json(&state).map_err(|err| conv(6, err))?,
        summary: row.get(7)?,
        open_questions: json::from_json(&open_questions).map_err(|err| conv(8, err))?,
        next_steps: json::from_json(&next_steps).map_err(|err| conv(9, err))?,
        files_touched: json::from_json(&files_touched).map_err(|err| conv(10, err))?,
        source_checkpoint: source_checkpoint.map(director_domain::ids::CheckpointId::from_string),
        source_task_version: source_task_version.map(|v| v.max(0) as u64),
        created_at: json::parse_timestamp(&row.get::<_, String>(13)?)
            .map_err(|err| conv(13, err))?,
        accepted_at: accepted_at
            .map(|text| json::parse_timestamp(&text))
            .transpose()
            .map_err(|err| conv(14, err))?,
        accepted_by_session: accepted_by_session.map(SessionId::from_string),
        cause: cause
            .map(|text| json::from_json(&text))
            .transpose()
            .map_err(|err| conv(16, err))?,
    })
}

/// A bad column is an error, not a default.
fn conv(index: usize, err: StoreError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(err))
}
