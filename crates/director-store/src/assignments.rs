//! [`AssignmentRepository`] over SQLite.
//!
//! This module holds the task-outlives-agent invariant in storage, and the
//! mechanism is the append-only release: ending an assignment marks the row
//! `released`, it never deletes it. Reassigning TASK-42 from Claude to Codex
//! therefore leaves Claude's tenure intact and queryable, which is what makes
//! "who worked on this task, and why did each one stop" an ordinary query.
//!
//! ## The atomic assignment operation
//!
//! [`assign_task`] is the one place several facts must land together or not at
//! all:
//!
//! 1. any currently-active assignment for the task is released with the reason
//!    `Reassigned`;
//! 2. the departing agent's denormalized `current_task` view is cleared;
//! 3. the new assignment row is inserted as `active`;
//! 4. the incoming agent's `current_task` view moves to this task.
//!
//! All four are one transaction. If the agent does not exist, the transaction
//! fails on the foreign key and nothing is written — the sitting agent keeps
//! the task, rather than the task being left with no one.
//!
//! Step 2 runs before step 4 deliberately. Reassigning a task to the agent that
//! already holds it clears and then re-sets the same `current_task`, and in the
//! other order the clear would win and the view would end up stale.
//!
//! The "at most one active assignment per task" rule is a partial unique index
//! in the schema, so a concurrent double-assign is rejected by the database
//! itself rather than by a check-then-act race in this code.

use async_trait::async_trait;
use rusqlite::OptionalExtension;

use director_domain::assignment::{AgentAssignment, AssignmentStatus, ReleaseReason};
use director_domain::ids::{AgentId, AssignmentId, SessionId, TaskId};
use director_domain::StoreError;

use crate::connection::{row_count_to_outcome, translate_error, PooledConn};
use crate::json;

/// The store-facing implementation of [`AssignmentRepository`].
#[derive(Clone)]
pub struct SqliteAssignmentRepository {
    pool: crate::connection::ConnectionPool,
}

impl SqliteAssignmentRepository {
    pub(crate) fn new(pool: crate::connection::ConnectionPool) -> Self {
        SqliteAssignmentRepository { pool }
    }

    fn conn(&self) -> PooledConn {
        self.pool.get()
    }
}

#[async_trait]
impl director_domain::Store for SqliteAssignmentRepository {
    type Error = StoreError;
}

#[async_trait]
impl director_domain::AssignmentRepository for SqliteAssignmentRepository {
    async fn create_assignment(
        &self,
        assignment: &AgentAssignment,
    ) -> Result<AgentAssignment, StoreError> {
        let assignment = assignment.clone();
        let conn = self.conn();
        insert_assignment(&conn, &assignment)?;
        load_assignment(&conn, &assignment.id)
    }

    async fn get_assignment(&self, id: &AssignmentId) -> Result<AgentAssignment, StoreError> {
        let conn = self.conn();
        load_assignment(&conn, id)
    }

    async fn update_assignment(
        &self,
        assignment: &AgentAssignment,
    ) -> Result<AgentAssignment, StoreError> {
        let assignment = assignment.clone();
        let conn = self.conn();
        // The assignment's lifecycle moves (`activate`, `release`) bump the
        // version in the domain, so the caller's version is already the version
        // they intend to store. The precondition is that the row is exactly one
        // behind them: a caller who read an older row, or who raced a writer,
        // matches nothing and gets a conflict instead of a silent overwrite.
        let rows = conn
            .execute(
                "UPDATE agent_assignments
                    SET session_id = ?2, status = ?3, released_at = ?4, release_reason = ?5,
                        note = ?6, state_version = ?7
                  WHERE id = ?1 AND state_version = ?7 - 1",
                rusqlite::params![
                    assignment.id.as_str(),
                    assignment.session_id.as_ref().map(SessionId::as_str),
                    json::to_json(&assignment.status)?,
                    assignment.released_at.map(json::timestamp),
                    assignment
                        .release_reason
                        .map(|r| json::to_json(&r))
                        .transpose()?,
                    assignment.note,
                    assignment.state_version as i64,
                ],
            )
            .map_err(translate_error)?;
        row_count_to_outcome(
            rows,
            "assignment",
            assignment.id.as_str(),
            assignment.state_version,
        )?;
        load_assignment(&conn, &assignment.id)
    }

    async fn release_assignment(
        &self,
        id: &AssignmentId,
        reason: ReleaseReason,
        caller_version: u64,
    ) -> Result<AgentAssignment, StoreError> {
        let conn = self.conn();
        let rows = conn
            .execute(
                "UPDATE agent_assignments
                    SET status = ?2, released_at = ?3, release_reason = ?4,
                        state_version = state_version + 1
                  WHERE id = ?1 AND state_version = ?5",
                rusqlite::params![
                    id.as_str(),
                    json::to_json(&AssignmentStatus::Released)?,
                    json::timestamp(chrono::Utc::now()),
                    json::to_json(&reason)?,
                    caller_version as i64,
                ],
            )
            .map_err(translate_error)?;
        row_count_to_outcome(rows, "assignment", id.as_str(), caller_version)?;

        // The released assignment's agent no longer holds this task. The
        // assignment row is the authority; this clears the denormalized view it
        // reflects, tolerating an agent row that has since moved on.
        let released = load_assignment(&conn, id)?;
        clear_current_task_if_still(&conn, &released.agent_id, &released.task_id)?;
        Ok(released)
    }

    async fn active_assignment_for_task(
        &self,
        task: &TaskId,
    ) -> Result<Option<AgentAssignment>, StoreError> {
        let conn = self.conn();
        conn.query_row(
            "SELECT id, task_id, agent_id, session_id, status, assigned_at, released_at,
                    release_reason, note, state_version
             FROM agent_assignments WHERE task_id = ?1 AND status = ?2",
            rusqlite::params![task.as_str(), json::to_json(&AssignmentStatus::Active)?],
            row_to_assignment,
        )
        .optional()
        .map_err(translate_error)
    }

    async fn assignment_history(&self, task: &TaskId) -> Result<Vec<AgentAssignment>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, task_id, agent_id, session_id, status, assigned_at, released_at,
                        release_reason, note, state_version
                 FROM agent_assignments WHERE task_id = ?1 ORDER BY assigned_at",
            )
            .map_err(translate_error)?;
        let rows = stmt
            .query_map([task.as_str()], row_to_assignment)
            .map_err(translate_error)?;
        let mut all = Vec::new();
        for row in rows {
            all.push(row.map_err(translate_error)?);
        }
        Ok(all)
    }
}

/// The atomic assignment operation: release any sitting tenant, insert the new
/// assignment as active, and move the agent's `current_task` view — one
/// transaction, all or nothing.
///
/// This is the method a caller uses to *assign* a task. The trait's
/// `create_assignment` inserts a row in any status (a `Proposed` assignment
/// that is not yet acknowledged); this is the operation that actually hands the
/// task to an agent.
pub async fn assign_task(
    pool: &crate::connection::ConnectionPool,
    task_id: &TaskId,
    agent_id: &AgentId,
    assignment_id: &AssignmentId,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<AgentAssignment, StoreError> {
    let mut conn = pool.get();
    let tx = conn.transaction().map_err(translate_error)?;

    // 1. Who is being displaced, if anyone? This has to be learned *before* the
    //    release in step 2, because afterwards the row no longer carries the
    //    `active` status that identifies it as the sitting tenant.
    let sitting: Option<AgentId> = tx
        .query_row(
            "SELECT agent_id FROM agent_assignments
              WHERE task_id = ?1 AND status = ?2",
            rusqlite::params![task_id.as_str(), json::to_json(&AssignmentStatus::Active)?],
            |row| row.get::<_, String>(0).map(AgentId::from_string),
        )
        .optional()
        .map_err(translate_error)?;

    // 2. Release the sitting tenant, if there is one. Its tenure is retained —
    //    the row is marked, never deleted — so the history stays complete.
    //    Status is matched and written as JSON, the way every other write does.
    //    This must precede the insert in step 3: the partial unique index allows
    //    one active assignment per task, and releasing first is what keeps it
    //    from firing on the handoff.
    tx.execute(
        "UPDATE agent_assignments
            SET status = ?4, released_at = ?2,
                release_reason = ?3, state_version = state_version + 1
          WHERE task_id = ?1 AND status = ?5",
        rusqlite::params![
            task_id.as_str(),
            json::timestamp(now),
            json::to_json(&ReleaseReason::Reassigned)?,
            json::to_json(&AssignmentStatus::Released)?,
            json::to_json(&AssignmentStatus::Active)?,
        ],
    )
    .map_err(translate_error)?;

    // 3. The departing agent's denormalized view is cleared *before* the new
    //    agent's is set. The order matters for the same-agent case: clear, then
    //    point at the task. Clearing only if the view still names this task
    //    keeps a late release of an older assignment from undoing a newer move.
    if let Some(departing) = &sitting {
        clear_current_task_if_still(&tx, departing, task_id)?;
    }

    // 4. The new assignment. Status is `active`: this operation is the handoff.
    //    A `Proposed` assignment is a separate, non-displacing insert.
    tx.execute(
        "INSERT INTO agent_assignments (id, task_id, agent_id, status, assigned_at,
                                        state_version)
         VALUES (?1, ?2, ?3, ?4, ?5, 1)",
        rusqlite::params![
            assignment_id.as_str(),
            task_id.as_str(),
            agent_id.as_str(),
            json::to_json(&AssignmentStatus::Active)?,
            json::timestamp(now),
        ],
    )
    .map_err(translate_error)?;

    // 5. The incoming agent's denormalized view. If the agent row does not
    //    exist the foreign key on the insert in step 4 fails and the whole
    //    transaction rolls back — leaving the sitting agent with the task,
    //    which is the safe failure mode.
    tx.execute(
        "UPDATE agents SET current_task = ?2, updated_at = ?3, state_version = state_version + 1
          WHERE id = ?1",
        rusqlite::params![agent_id.as_str(), task_id.as_str(), json::timestamp(now)],
    )
    .map_err(translate_error)?;

    tx.commit().map_err(translate_error)?;

    let assignment = AgentAssignment {
        id: assignment_id.clone(),
        task_id: task_id.clone(),
        agent_id: agent_id.clone(),
        session_id: None,
        status: AssignmentStatus::Active,
        assigned_at: now,
        released_at: None,
        release_reason: None,
        note: None,
        state_version: 1,
    };
    Ok(assignment)
}

/// Insert an assignment row in whatever status it carries.
pub(crate) fn insert_assignment(
    conn: &PooledConn,
    assignment: &AgentAssignment,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO agent_assignments (id, task_id, agent_id, session_id, status,
                                        assigned_at, released_at, release_reason, note,
                                        state_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            assignment.id.as_str(),
            assignment.task_id.as_str(),
            assignment.agent_id.as_str(),
            assignment.session_id.as_ref().map(SessionId::as_str),
            json::to_json(&assignment.status)?,
            json::timestamp(assignment.assigned_at),
            assignment.released_at.map(json::timestamp),
            assignment
                .release_reason
                .map(|r| json::to_json(&r))
                .transpose()?,
            assignment.note,
            assignment.state_version as i64,
        ],
    )
    .map_err(translate_error)?;
    Ok(())
}

/// Load one assignment by id, or [`StoreError::NotFound`].
pub(crate) fn load_assignment(
    conn: &PooledConn,
    id: &AssignmentId,
) -> Result<AgentAssignment, StoreError> {
    conn.query_row(
        "SELECT id, task_id, agent_id, session_id, status, assigned_at, released_at,
                release_reason, note, state_version
         FROM agent_assignments WHERE id = ?1",
        [id.as_str()],
        row_to_assignment,
    )
    .map_err(translate_error)
}

/// Clear an agent's `current_task` view, but only if it still points at this
/// task. A reassignment that already moved the agent on must not be undone by a
/// late release of an older assignment.
///
/// Takes anything that derefs to a [`rusqlite::Connection`] so it can run
/// either standalone (a release) or inside the transaction that a reassignment
/// commits atomically with the rest of the handoff.
fn clear_current_task_if_still(
    conn: &impl std::ops::Deref<Target = rusqlite::Connection>,
    agent_id: &AgentId,
    task_id: &TaskId,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE agents
            SET current_task = NULL, updated_at = ?3, state_version = state_version + 1
          WHERE id = ?1 AND current_task = ?2",
        rusqlite::params![
            agent_id.as_str(),
            task_id.as_str(),
            json::timestamp(chrono::Utc::now()),
        ],
    )
    .map_err(translate_error)?;
    Ok(())
}

fn row_to_assignment(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentAssignment> {
    let session_id: Option<String> = row.get(3)?;
    let assigned_at: String = row.get(5)?;
    let released_at: Option<String> = row.get(6)?;
    let release_reason: Option<String> = row.get(7)?;
    let note: Option<String> = row.get(8)?;

    let status: AssignmentStatus =
        json::from_json(&row.get::<_, String>(4)?).map_err(|err| sqlite_conv_failure(4, err))?;
    let mut assignment = AgentAssignment::propose(
        AssignmentId::from_string(row.get::<_, String>(0)?),
        TaskId::from_string(String::new()),
        AgentId::from_string(String::new()),
    );
    assignment.task_id = TaskId::from_string(row.get::<_, String>(1)?);
    assignment.agent_id = AgentId::from_string(row.get::<_, String>(2)?);
    assignment.session_id = session_id.map(SessionId::from_string);
    assignment.status = status;
    assignment.assigned_at =
        json::parse_timestamp(&assigned_at).map_err(|err| sqlite_conv_failure(5, err))?;
    assignment.released_at = released_at
        .map(|text| json::parse_timestamp(&text))
        .transpose()
        .map_err(|err| sqlite_conv_failure(6, err))?;
    assignment.release_reason = release_reason
        .map(|text| json::from_json(&text))
        .transpose()
        .map_err(|err| sqlite_conv_failure(7, err))?;
    assignment.note = note;
    assignment.state_version = row.get::<_, i64>(9)? as u64;
    Ok(assignment)
}

fn sqlite_conv_failure(index: usize, err: StoreError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, err.into())
}
