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

use director_domain::agent::AgentStatus;
use director_domain::assignment::{AgentAssignment, AssignmentStatus, ReleaseReason};
use director_domain::ids::{AgentId, AssignmentId, SessionId, TaskId};
use director_domain::session::{AgentSession, SessionEnd, SessionStatus};
use director_domain::task::{Task, TaskStatus};
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

/// The handoff itself, run inside the caller's transaction: release any sitting
/// tenant, clear their `current_task` view, insert the new assignment as
/// active, and point the incoming agent's view at the task. Returns the agent
/// that was displaced, if any, so the caller can report the move.
///
/// Extracted because two operations need this exact sequence. [`assign_task`]
/// commits it on its own; [`assign_and_start`] folds the task's status move into
/// the same transaction, so an assignment never lands without the task
/// reflecting it — and a failure partway through never lands at all.
fn handoff(
    tx: &rusqlite::Transaction<'_>,
    task_id: &TaskId,
    agent_id: &AgentId,
    assignment_id: &AssignmentId,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<AgentId>, StoreError> {
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
        clear_current_task_if_still(tx, departing, task_id)?;
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

    Ok(sitting)
}

/// The assignment row as [`handoff`] leaves it: active, with no session yet,
/// starting at version 1. A session arrives when the agent acknowledges the
/// work, which is the MONITOR step's concern rather than the handoff's.
fn assignment_row(
    task_id: &TaskId,
    agent_id: &AgentId,
    assignment_id: &AssignmentId,
    now: chrono::DateTime<chrono::Utc>,
) -> AgentAssignment {
    AgentAssignment {
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
    handoff(&tx, task_id, agent_id, assignment_id, now)?;
    tx.commit().map_err(translate_error)?;

    Ok(assignment_row(task_id, agent_id, assignment_id, now))
}

/// [`assign_task`], and the task starts: the same atomic handoff, plus the
/// task's move to `in_progress` and the status transition that records it — all
/// one transaction.
///
/// This is the operation the loop's ASSIGN step uses. Splitting the handoff
/// from the status move across two store calls would leave a window in which
/// the task was assigned but still read `todo`, which is precisely the state a
/// crash between the two calls would leave behind. Folding them together makes
/// "an assigned task is an in-progress task" a guarantee of the write rather
/// than a caller's ordering.
///
/// Returns the assignment, the task as it now stands, and the agent the handoff
/// displaced — all read back from the store rather than echoed from the input.
pub async fn assign_and_start(
    pool: &crate::connection::ConnectionPool,
    task_id: &TaskId,
    agent_id: &AgentId,
    assignment_id: &AssignmentId,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(AgentAssignment, Task, Option<AgentId>), StoreError> {
    let mut conn = pool.get();
    let tx = conn.transaction().map_err(translate_error)?;

    // The status the task is moving from, read inside the transaction so the
    // transition history records what the store actually saw — not what the
    // caller believed when it decided to assign. A task that is not `todo` is
    // not started here; the handoff still happens, and the history records the
    // real transition, so the record stays honest even if a concurrent writer
    // moved the task between the caller's validation and this write.
    let previous_status: Option<TaskStatus> = tx
        .query_row(
            "SELECT status FROM tasks WHERE id = ?1",
            [task_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(translate_error)?
        .map(|text| json::from_json::<TaskStatus>(&text))
        .transpose()?;

    let sitting = handoff(&tx, task_id, agent_id, assignment_id, now)?;

    // The task reflects the assignment: an agent is expected to be working it.
    // Version is bumped by the write itself, the way `update_task` does it, so
    // the caller's read of the task does not become a stale-write hazard here.
    tx.execute(
        "UPDATE tasks
            SET status = ?2, state_version = state_version + 1, updated_at = ?3
          WHERE id = ?1",
        rusqlite::params![
            task_id.as_str(),
            json::to_json(&TaskStatus::InProgress)?,
            json::timestamp(now),
        ],
    )
    .map_err(translate_error)?;

    // The transition, recorded the same way `update_task` records one: same
    // transaction as the update, and only when the status genuinely changed, so
    // the history never accumulates no-op entries.
    if previous_status != Some(TaskStatus::InProgress) {
        tx.execute(
            "INSERT INTO task_status_history (task_id, from_status, to_status, occurred_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                task_id.as_str(),
                json::to_json_or_null(previous_status.as_ref())?,
                json::to_json(&TaskStatus::InProgress)?,
                json::timestamp(now),
            ],
        )
        .map_err(translate_error)?;
    }

    tx.commit().map_err(translate_error)?;

    let assignment = load_assignment(&conn, assignment_id)?;
    let task = crate::tasks::load_task(&conn, task_id)?;
    Ok((assignment, task, sitting))
}

/// Reclaim a task whose holder has gone quiet: release the assignment as
/// `LeaseExpired`, close the session that held it as `Vanished` if it was still
/// live, record the agent as `Disconnected`, clear its `current_task` view, and
/// put the task back to `todo` so a later ASSIGN round can hand it out again —
/// one transaction, all or nothing.
///
/// This is the operation the loop's MONITOR step uses once a heartbeat has been
/// quiet past the stale window. Ending a lease is five separate facts that must
/// land together, and the reason each one is in this transaction is the same as
/// the reason the handoff and the status move are one write in
/// [`assign_and_start`]: the states a partial write would strand are invisible
/// to a reader. An assignment released but a task still `in_progress` looks
/// like work in flight with no one on it; a task back to `todo` with the
/// assignment still `active` looks handable and held at the same time. Folding
/// them together makes "an expired lease is a `todo` task with a retained
/// tenure" a property of the write rather than an ordering the caller has to
/// get right.
///
/// Returns `None` when no assignment holds the task at write time. That is the
/// race a concurrent release or expiry wins: nothing is reclaimed, because
/// there was nothing to reclaim, and the caller reports the state it found
/// rather than a write it did not make.
pub async fn expire_lease(
    pool: &crate::connection::ConnectionPool,
    task_id: &TaskId,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<(AgentAssignment, Task, Option<AgentSession>)>, StoreError> {
    let mut conn = pool.get();
    let tx = conn.transaction().map_err(translate_error)?;

    // 1. The lease in force, read before anything is written because after the
    //    release the row no longer carries the `active` status that identifies
    //    it. A task with no active assignment has no lease to expire; the round
    //    reports it as orphaned rather than inventing one.
    let assignment: Option<AgentAssignment> = tx
        .query_row(
            "SELECT id, task_id, agent_id, session_id, status, assigned_at, released_at,
                    release_reason, note, state_version
             FROM agent_assignments WHERE task_id = ?1 AND status = ?2",
            rusqlite::params![task_id.as_str(), json::to_json(&AssignmentStatus::Active)?],
            row_to_assignment,
        )
        .optional()
        .map_err(translate_error)?;
    let Some(assignment) = assignment else {
        // Commit the empty transaction so the connection comes back to the pool
        // clean rather than dropped mid-transaction.
        tx.commit().map_err(translate_error)?;
        return Ok(None);
    };

    // 2. The status the task is moving from, read inside the transaction so the
    //    history records what the store saw — not what the caller believed when
    //    it decided to expire the lease.
    let previous_status: Option<TaskStatus> = tx
        .query_row(
            "SELECT status FROM tasks WHERE id = ?1",
            [task_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(translate_error)?
        .map(|text| json::from_json::<TaskStatus>(&text))
        .transpose()?;

    // 3. Release the assignment. The tenure is retained — the row is marked,
    //    never deleted — and the reason says the lease ran out, which is what
    //    distinguishes this from a deliberate reassignment further down the
    //    task's history.
    tx.execute(
        "UPDATE agent_assignments
            SET status = ?2, released_at = ?3, release_reason = ?4,
                state_version = state_version + 1
          WHERE id = ?1",
        rusqlite::params![
            assignment.id.as_str(),
            json::to_json(&AssignmentStatus::Released)?,
            json::timestamp(now),
            json::to_json(&ReleaseReason::LeaseExpired)?,
        ],
    )
    .map_err(translate_error)?;

    // 4. The session, closed as `Vanished` if it was still live. A session that
    //    already closed is left exactly as it was: its record says how it ended,
    //    and a lease expiry that raced a clean close must not rewrite the
    //    better evidence.
    let mut session = None;
    if let Some(session_id) = assignment.session_id.as_ref() {
        let held: Option<(i64, String)> = tx
            .query_row(
                "SELECT state_version, status FROM agent_sessions WHERE id = ?1",
                [session_id.as_str()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(translate_error)?;
        if let Some((version, status)) = held {
            if json::from_json::<SessionStatus>(&status)?.is_live() {
                crate::sessions::close_session(
                    &tx,
                    session_id,
                    SessionEnd::Vanished,
                    now,
                    version as u64,
                )?;
                session = Some(crate::sessions::load_session(&tx, session_id)?);
            }
        }
    }

    // 5. The agent no longer holds the task. The assignment row is the
    //    authority; this clears the denormalized view it reflects, tolerating
    //    an agent row that has since moved on to other work.
    clear_current_task_if_still(&tx, &assignment.agent_id, task_id)?;

    // 6. The agent is recorded as gone, so the loop does not hand it right back
    //    the work it just lost. An operator's `Offline` is a stronger statement
    //    than a heartbeat's absence, and it is left standing.
    tx.execute(
        "UPDATE agents
            SET status = ?2, updated_at = ?3, state_version = state_version + 1
          WHERE id = ?1 AND status <> ?4",
        rusqlite::params![
            assignment.agent_id.as_str(),
            json::to_json(&AgentStatus::Disconnected)?,
            json::timestamp(now),
            json::to_json(&AgentStatus::Offline)?,
        ],
    )
    .map_err(translate_error)?;

    // 7. The task is handable again. An expired lease is not a verdict on the
    //    work: the task is unfinished, not failed, so it goes back to `todo`
    //    where the next round's `ready_tasks` can find it.
    tx.execute(
        "UPDATE tasks
            SET status = ?2, state_version = state_version + 1, updated_at = ?3
          WHERE id = ?1",
        rusqlite::params![
            task_id.as_str(),
            json::to_json(&TaskStatus::Todo)?,
            json::timestamp(now),
        ],
    )
    .map_err(translate_error)?;

    // 8. The transition, recorded the same way `assign_and_start` records one:
    //    same transaction as the update, and only when the status genuinely
    //    changed, so the history never accumulates no-op entries.
    if previous_status != Some(TaskStatus::Todo) {
        tx.execute(
            "INSERT INTO task_status_history (task_id, from_status, to_status, occurred_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                task_id.as_str(),
                json::to_json_or_null(previous_status.as_ref())?,
                json::to_json(&TaskStatus::Todo)?,
                json::timestamp(now),
            ],
        )
        .map_err(translate_error)?;
    }

    tx.commit().map_err(translate_error)?;

    let assignment = load_assignment(&conn, &assignment.id)?;
    let task = crate::tasks::load_task(&conn, task_id)?;
    Ok(Some((assignment, task, session)))
}

/// Record that the agent holding an assignment began the session doing the
/// work: insert the session row and attach it to the assignment — one
/// transaction, all or nothing.
///
/// This is the operation the loop's MONITOR step uses when an agent it assigned
/// work to reports that it has started. Until it runs, the assignment is a hand
/// Orqyn recorded but no evidence that the work ever began; after it runs, the
/// tenure has a session behind it, which is what a later lease expiry closes as
/// `Vanished` and what makes "which invocation of which agent did this task"
/// answerable.
///
/// The assignment must still be active. Attaching a session to a released
/// assignment would give a ended tenure evidence of work it never did, so the
/// write is guarded on the status and refuses rather than repairing.
pub async fn acknowledge_assignment(
    pool: &crate::connection::ConnectionPool,
    assignment_id: &AssignmentId,
    session: &AgentSession,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(AgentAssignment, AgentSession), StoreError> {
    let mut conn = pool.get();
    let tx = conn.transaction().map_err(translate_error)?;

    // The session row first: the assignment's `session_id` column references
    // it, so the row has to exist before the reference can point at it, and a
    // failure here leaves nothing behind.
    crate::sessions::insert_session(&tx, session)?;

    // The link, guarded on the assignment still being active. Zero rows means
    // the assignment was released between the caller's read and this write, and
    // the whole transaction rolls back rather than leaving an orphan session
    // attached to nothing: the evidence trail is one unit with the tenure it
    // describes.
    let rows = tx
        .execute(
            "UPDATE agent_assignments
                SET session_id = ?2, state_version = state_version + 1
              WHERE id = ?1 AND status = ?3",
            rusqlite::params![
                assignment_id.as_str(),
                session.id.as_str(),
                json::to_json(&AssignmentStatus::Active)?,
            ],
        )
        .map_err(translate_error)?;
    if rows == 0 {
        // The constraint is the assignment's own status, which the caller
        // cannot have known was about to change; a version conflict would claim
        // a version the caller never held.
        return Err(StoreError::ConstraintViolation(format!(
            "assignment {} is not active, so it cannot acknowledge a session",
            assignment_id
        )));
    }

    // The agent is working. Until this point its status said nothing about the
    // handoff, because a handoff Orqyn recorded is not evidence the agent began;
    // a session is. An operator's `Offline` is left standing, for the same
    // reason an expiry leaves it standing: it is a stronger statement than the
    // agent's own report.
    tx.execute(
        "UPDATE agents
            SET status = ?2, updated_at = ?3, state_version = state_version + 1
          WHERE id = ?1 AND status <> ?4",
        rusqlite::params![
            session.agent_id.as_str(),
            json::to_json(&AgentStatus::Busy)?,
            json::timestamp(now),
            json::to_json(&AgentStatus::Offline)?,
        ],
    )
    .map_err(translate_error)?;

    tx.commit().map_err(translate_error)?;

    let assignment = load_assignment(&conn, assignment_id)?;
    let session = crate::sessions::load_session(&conn, &session.id)?;
    Ok((assignment, session))
}

/// Record that the agent holding a tenure finished the work it was handed: the
/// task moves to `verification_pending`, the tenure is released as
/// `WorkComplete`, the session that did the work is closed as `Clean` if it
/// was still live, the agent's `current_task` view is cleared, and an agent
/// that was `Busy` on this work is `Available` again — one transaction, all or
/// nothing.
///
/// This is the operation the loop's MONITOR step uses when an agent reports it
/// has finished. The task is deliberately *not* moved to `done`: nothing
/// self-reports completion in Orqyn, and `done` is a verdict only the
/// verification engine reaches. Ending the tenure here is what keeps "the
/// agent's part is over" distinct from "the work is verified" — the task is
/// now Orqyn's to judge, and the agent is free to take other work while it is
/// judged.
///
/// The tenure is matched by id *and* by still being active, in that order. A
/// reassignment that landed between the caller's read and this write would put
/// a different tenure behind the same task, and matching on the task alone
/// would end the newcomer's tenure with someone else's completion — so the
/// release is what decides whether there is anything to report, and a
/// concurrently-ended tenure reports `None` rather than reaching for the task.
///
/// The agent is moved to `Available` only if it was `Busy`. A heartbeat that
/// says the agent went quiet or gone is left standing, because a completion
/// report is not a heartbeat: the registry's own evidence about the agent is
/// the authority on the agent, and this write is about the task.
///
/// Returns `None` when the tenure is not active at write time. That is the race
/// a concurrent release or expiry wins: nothing is ended, because there was no
/// tenure to end.
pub async fn report_completion(
    pool: &crate::connection::ConnectionPool,
    assignment_id: &AssignmentId,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<(AgentAssignment, Task, Option<AgentSession>)>, StoreError> {
    let mut conn = pool.get();
    let tx = conn.transaction().map_err(translate_error)?;

    // 1. The tenure as it stands, matched on the id the caller named *and* on
    //    still being active. Read before anything is written because after the
    //    release the row no longer carries the `active` status that identifies
    //    it, and because the rest of the transaction works from the task,
    //    agent, and session this tenure names rather than from the caller's
    //    belief about them.
    let assignment: Option<AgentAssignment> = tx
        .query_row(
            "SELECT id, task_id, agent_id, session_id, status, assigned_at, released_at,
                    release_reason, note, state_version
             FROM agent_assignments WHERE id = ?1 AND status = ?2",
            rusqlite::params![
                assignment_id.as_str(),
                json::to_json(&AssignmentStatus::Active)?
            ],
            row_to_assignment,
        )
        .optional()
        .map_err(translate_error)?;
    let Some(assignment) = assignment else {
        // Commit the empty transaction so the connection comes back to the pool
        // clean rather than dropped mid-transaction.
        tx.commit().map_err(translate_error)?;
        return Ok(None);
    };

    // 2. End the tenure, guarded on it still being active. This is the first
    //    write, so it is what takes the write lock, and its row count is what
    //    decides whether there is anything to report: a tenure a concurrent
    //    writer ended between step 1 and here matches nothing, and the whole
    //    report is abandoned rather than reaching for a task this tenure no
    //    longer holds.
    let rows = tx
        .execute(
            "UPDATE agent_assignments
                SET status = ?2, released_at = ?3, release_reason = ?4,
                    state_version = state_version + 1
              WHERE id = ?1 AND status = ?5",
            rusqlite::params![
                assignment.id.as_str(),
                json::to_json(&AssignmentStatus::Released)?,
                json::timestamp(now),
                json::to_json(&ReleaseReason::WorkComplete)?,
                json::to_json(&AssignmentStatus::Active)?,
            ],
        )
        .map_err(translate_error)?;
    if rows == 0 {
        tx.commit().map_err(translate_error)?;
        return Ok(None);
    }

    // 3. The status the task is moving from, read inside the transaction so the
    //    history records what the store saw — not what the caller believed when
    //    it decided the work was done.
    let previous_status: Option<TaskStatus> = tx
        .query_row(
            "SELECT status FROM tasks WHERE id = ?1",
            [assignment.task_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(translate_error)?
        .map(|text| json::from_json::<TaskStatus>(&text))
        .transpose()?;

    // 4. The task is awaiting Orqyn's verdict. `verification_pending` is as far
    //    as an agent's own report can reach; `done` is the verification
    //    engine's to write.
    tx.execute(
        "UPDATE tasks
            SET status = ?2, state_version = state_version + 1, updated_at = ?3
          WHERE id = ?1",
        rusqlite::params![
            assignment.task_id.as_str(),
            json::to_json(&TaskStatus::VerificationPending)?,
            json::timestamp(now),
        ],
    )
    .map_err(translate_error)?;

    // 5. The session, closed as `Clean` if it was still live. The agent
    //    reported finishing, which is a clean end from its side — and an
    //    expiry that races this report must not overwrite it with a
    //    disappearance it inferred, so a session that already closed is left
    //    exactly as it was.
    let mut session = None;
    if let Some(session_id) = assignment.session_id.as_ref() {
        let held: Option<(i64, String)> = tx
            .query_row(
                "SELECT state_version, status FROM agent_sessions WHERE id = ?1",
                [session_id.as_str()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(translate_error)?;
        if let Some((version, status)) = held {
            if json::from_json::<SessionStatus>(&status)?.is_live() {
                crate::sessions::close_session(
                    &tx,
                    session_id,
                    SessionEnd::Clean,
                    now,
                    version as u64,
                )?;
                session = Some(crate::sessions::load_session(&tx, session_id)?);
            }
        }
    }

    // 6. The agent no longer holds this task. The assignment row is the
    //    authority; this clears the denormalized view it reflects, tolerating
    //    an agent row that has since moved on to other work.
    clear_current_task_if_still(&tx, &assignment.agent_id, &assignment.task_id)?;

    // 7. An agent that was working this task is free again — but only if it was
    //    working. A heartbeat that already said the agent went quiet or gone is
    //    the stronger statement about the agent, and it is left standing.
    tx.execute(
        "UPDATE agents
            SET status = ?2, updated_at = ?3, state_version = state_version + 1
          WHERE id = ?1 AND status = ?4",
        rusqlite::params![
            assignment.agent_id.as_str(),
            json::to_json(&AgentStatus::Available)?,
            json::timestamp(now),
            json::to_json(&AgentStatus::Busy)?,
        ],
    )
    .map_err(translate_error)?;

    // 8. The transition, recorded the same way every other status move records
    //    one: same transaction as the update, and only when the status
    //    genuinely changed, so the history never accumulates no-op entries.
    if previous_status != Some(TaskStatus::VerificationPending) {
        tx.execute(
            "INSERT INTO task_status_history (task_id, from_status, to_status, occurred_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                assignment.task_id.as_str(),
                json::to_json_or_null(previous_status.as_ref())?,
                json::to_json(&TaskStatus::VerificationPending)?,
                json::timestamp(now),
            ],
        )
        .map_err(translate_error)?;
    }

    tx.commit().map_err(translate_error)?;

    let assignment = load_assignment(&conn, &assignment.id)?;
    let task = crate::tasks::load_task(&conn, &assignment.task_id)?;
    Ok(Some((assignment, task, session)))
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
