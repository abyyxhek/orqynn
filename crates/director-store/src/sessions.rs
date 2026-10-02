//! [`SessionRepository`] over SQLite.
//!
//! A session is one invocation of one agent on one machine, and the point of
//! persisting it is the lineage: a task accumulates sessions over its life, and
//! "who worked on TASK-42, when, and how did each stint end" is a query on
//! `sessions_for_task`, not a reconstruction from memory.
//!
//! `end_session` closes a session transactionally with its version bump, so a
//! session is never recorded as closed without the version that closed it.

use async_trait::async_trait;
use rusqlite::OptionalExtension;

use director_domain::ids::{AgentId, MachineId, ProjectId, SessionId, TaskId};
use director_domain::session::{AgentSession, SessionEnd, SessionStatus};
use director_domain::StoreError;

use crate::connection::{row_count_to_outcome, translate_error, PooledConn};
use crate::json;

/// The store-facing implementation of [`SessionRepository`].
#[derive(Clone)]
pub struct SqliteSessionRepository {
    pool: crate::connection::ConnectionPool,
}

impl SqliteSessionRepository {
    pub(crate) fn new(pool: crate::connection::ConnectionPool) -> Self {
        SqliteSessionRepository { pool }
    }

    fn conn(&self) -> PooledConn {
        self.pool.get()
    }
}

#[async_trait]
impl director_domain::Store for SqliteSessionRepository {
    type Error = StoreError;
}

#[async_trait]
impl director_domain::SessionRepository for SqliteSessionRepository {
    async fn create_session(&self, session: &AgentSession) -> Result<AgentSession, StoreError> {
        let session = session.clone();
        let conn = self.conn();
        insert_session(&conn, &session)?;
        load_session(&conn, &session.id)
    }

    async fn get_session(&self, id: &SessionId) -> Result<AgentSession, StoreError> {
        let conn = self.conn();
        load_session(&conn, id)
    }

    async fn update_session(&self, session: &AgentSession) -> Result<AgentSession, StoreError> {
        let session = session.clone();
        let conn = self.conn();
        let rows = conn
            .execute(
                "UPDATE agent_sessions
                    SET status = ?2, branch = ?3, commit_sha = ?4, ended_at = ?5, end = ?6,
                        workdir = ?7, last_seen = ?8, state_version = state_version + 1
                  WHERE id = ?1 AND state_version = ?9",
                rusqlite::params![
                    session.id.as_str(),
                    json::to_json(&session.status)?,
                    session.branch,
                    session.commit_sha,
                    session.ended_at.map(json::timestamp),
                    session.end.map(|e| json::to_json(&e)).transpose()?,
                    session.workdir,
                    session.last_seen.map(json::timestamp),
                    session.state_version as i64,
                ],
            )
            .map_err(translate_error)?;
        row_count_to_outcome(rows, "session", session.id.as_str(), session.state_version)?;
        load_session(&conn, &session.id)
    }

    async fn end_session(
        &self,
        id: &SessionId,
        end: SessionEnd,
        caller_version: u64,
    ) -> Result<AgentSession, StoreError> {
        let conn = self.conn();
        let rows = close_session(&conn, id, end, chrono::Utc::now(), caller_version)?;
        row_count_to_outcome(rows, "session", id.as_str(), caller_version)?;
        load_session(&conn, id)
    }

    async fn sessions_for_task(&self, task: &TaskId) -> Result<Vec<AgentSession>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, project_id, agent_id, machine_id, task_id, status,
                        parent_session_id, branch, commit_sha, started_at, ended_at, end,
                        workdir, last_seen, state_version
                 FROM agent_sessions WHERE task_id = ?1 ORDER BY started_at",
            )
            .map_err(translate_error)?;
        let rows = stmt
            .query_map([task.as_str()], row_to_session)
            .map_err(translate_error)?;
        let mut all = Vec::new();
        for row in rows {
            all.push(row.map_err(translate_error)?);
        }
        Ok(all)
    }
}

/// Insert a session row. Shared by `create_session` and by the lease
/// acknowledgment path, which starts a session in the same transaction as the
/// assignment it is attached to — the session and the link land together, or
/// neither does.
pub(crate) fn insert_session(
    conn: &impl std::ops::Deref<Target = rusqlite::Connection>,
    session: &AgentSession,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO agent_sessions (id, project_id, agent_id, machine_id, task_id, status,
                                     parent_session_id, branch, commit_sha, started_at,
                                     ended_at, end, workdir, last_seen, state_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        rusqlite::params![
            session.id.as_str(),
            session
                .project_id
                .as_ref()
                .map(ProjectId::as_str)
                .ok_or_else(|| StoreError::ConstraintViolation(format!(
                    "session {} has no project; Orqyn's store requires one",
                    session.id
                )))?,
            session.agent_id.as_str(),
            session.machine_id.as_str(),
            session.task_id.as_ref().map(TaskId::as_str),
            json::to_json(&session.status)?,
            session.parent_session_id.as_ref().map(SessionId::as_str),
            session.branch,
            session.commit_sha,
            json::timestamp(session.started_at),
            session.ended_at.map(json::timestamp),
            session.end.map(|e| json::to_json(&e)).transpose()?,
            session.workdir,
            session.last_seen.map(json::timestamp),
            session.state_version as i64,
        ],
    )
    .map_err(translate_error)?;
    Ok(())
}

/// Load one session by id, or [`StoreError::NotFound`].
///
/// Takes anything that derefs to a [`rusqlite::Connection`] so a caller can
/// reload a session inside the transaction it just wrote it in — a lease
/// expiry closes a session and reads the result back before it commits.
pub(crate) fn load_session(
    conn: &impl std::ops::Deref<Target = rusqlite::Connection>,
    id: &SessionId,
) -> Result<AgentSession, StoreError> {
    conn.query_row(
        "SELECT id, project_id, agent_id, machine_id, task_id, status,
                parent_session_id, branch, commit_sha, started_at, ended_at, end,
                workdir, last_seen, state_version
         FROM agent_sessions WHERE id = ?1",
        [id.as_str()],
        row_to_session,
    )
    .map_err(translate_error)
}

/// Close a session, recording how it ended and stamping the version that closed
/// it. Returns how many rows it closed.
///
/// The row count is returned rather than interpreted because the two callers
/// mean different things by a session that did not close:
///
/// - [`SessionRepository::end_session`](director_domain::SessionRepository::end_session)
///   turns zero rows into a [`StoreError::StateVersionConflict`], because a
///   caller who asked to close a session is owed the news that it is closed
///   already.
/// - A lease expiry checks the session's status before it calls this, and only
///   calls it for a session that is still live, so a session that closed on its
///   own is never reopened as a disappearance. Its record says how it ended,
///   and a clean close the agent managed to send is better evidence than the
///   one MONITOR inferred.
///
/// Takes anything that derefs to a [`rusqlite::Connection`] so a lease expiry
/// can close the session inside the transaction that releases the assignment it
/// belonged to — the two are one write, or neither happens.
pub(crate) fn close_session(
    conn: &impl std::ops::Deref<Target = rusqlite::Connection>,
    id: &SessionId,
    end: SessionEnd,
    now: chrono::DateTime<chrono::Utc>,
    expected_version: u64,
) -> Result<usize, StoreError> {
    let rows = conn
        .execute(
            "UPDATE agent_sessions
                SET status = ?2, end = ?3, ended_at = ?4,
                    last_seen = ?4, state_version = state_version + 1
              WHERE id = ?1 AND state_version = ?5",
            rusqlite::params![
                id.as_str(),
                // Written as JSON, the same as insert and update do, so the
                // read path's from_json sees the shape it expects.
                json::to_json(&SessionStatus::Closed)?,
                json::to_json(&end)?,
                json::timestamp(now),
                expected_version as i64,
            ],
        )
        .map_err(translate_error)?;
    Ok(rows)
}

/// Load a session if it exists, without failing when it does not.
pub(crate) fn find_session(
    conn: &PooledConn,
    id: &SessionId,
) -> Result<Option<AgentSession>, StoreError> {
    conn.query_row(
        "SELECT id, project_id, agent_id, machine_id, task_id, status,
                parent_session_id, branch, commit_sha, started_at, ended_at, end,
                workdir, last_seen, state_version
         FROM agent_sessions WHERE id = ?1",
        [id.as_str()],
        row_to_session,
    )
    .optional()
    .map_err(translate_error)
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentSession> {
    let project_id: Option<String> = row.get(1)?;
    let task_id: Option<String> = row.get(4)?;
    let parent_session_id: Option<String> = row.get(6)?;
    let started_at: String = row.get(9)?;
    let ended_at: Option<String> = row.get(10)?;

    let mut session = AgentSession::start(
        SessionId::from_string(row.get::<_, String>(0)?),
        project_id.map(ProjectId::from_string),
        AgentId::from_string(String::new()),
        MachineId::from_string(String::new()),
        None,
    );
    session.agent_id = AgentId::from_string(row.get::<_, String>(2)?);
    session.machine_id = MachineId::from_string(row.get::<_, String>(3)?);
    session.task_id = task_id.map(TaskId::from_string);
    session.status =
        json::from_json(&row.get::<_, String>(5)?).map_err(|err| sqlite_conv_failure(5, err))?;
    session.parent_session_id = parent_session_id.map(SessionId::from_string);
    session.branch = row.get(7)?;
    session.commit_sha = row.get(8)?;
    session.started_at =
        json::parse_timestamp(&started_at).map_err(|err| sqlite_conv_failure(9, err))?;
    session.ended_at = ended_at
        .map(|text| json::parse_timestamp(&text))
        .transpose()
        .map_err(|err| sqlite_conv_failure(10, err))?;
    session.end = row
        .get::<_, Option<String>>(11)?
        .map(|text| json::from_json(&text))
        .transpose()
        .map_err(|err| sqlite_conv_failure(11, err))?;
    session.workdir = row.get(12)?;
    session.last_seen = row
        .get::<_, Option<String>>(13)?
        .map(|text| json::parse_timestamp(&text))
        .transpose()
        .map_err(|err| sqlite_conv_failure(13, err))?;
    session.state_version = row.get::<_, i64>(14)? as u64;
    Ok(session)
}

fn sqlite_conv_failure(index: usize, err: StoreError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, err.into())
}
