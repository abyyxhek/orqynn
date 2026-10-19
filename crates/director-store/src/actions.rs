//! [`ActionRepository`] over SQLite.
//!
//! This is the durable log behind [`crate::context::RecentContext`]. The split
//! is deliberate: the table is complete and ordered, and the *window* a
//! replacement agent receives is bounded and compacted when it is built from
//! this. Keeping the bound on the read side means nothing is lost from the log
//! that a query might legitimately want, and the agent still cannot be handed an
//! unbounded window.
//!
//! Rows are never updated or deleted. An action is something that happened; the
//! log's job is to say so even if it later turns out to have been a mistake.

use async_trait::async_trait;

use director_domain::action::Action;
use director_domain::ids::{ActionId, AgentId, SessionId, TaskId};
use director_domain::StoreError;

use crate::connection::{translate_error, PooledConn};
use crate::json;

const ACTION_COLUMNS: &str = "id, task_id, agent_id, session_id, kind, summary, detail, at";

/// The store-facing implementation of [`ActionRepository`].
#[derive(Clone)]
pub struct SqliteActionRepository {
    pool: crate::connection::ConnectionPool,
}

impl SqliteActionRepository {
    pub(crate) fn new(pool: crate::connection::ConnectionPool) -> Self {
        SqliteActionRepository { pool }
    }

    fn conn(&self) -> PooledConn {
        self.pool.get()
    }
}

#[async_trait]
impl director_domain::Store for SqliteActionRepository {
    type Error = StoreError;
}

#[async_trait]
impl director_domain::ActionRepository for SqliteActionRepository {
    async fn record_action(&self, action: &Action) -> Result<Action, StoreError> {
        let action = action.clone();
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(translate_error)?;
        insert_action(&tx, &action)?;
        tx.commit().map_err(translate_error)?;
        drop(conn);

        let conn = self.conn();
        load_action(&conn, &action.id)
    }

    async fn get_action(&self, id: &ActionId) -> Result<Action, StoreError> {
        let conn = self.conn();
        load_action(&conn, id)
    }

    /// The task's action history, oldest first.
    ///
    /// `at` is the primary order and the only one that means wall-clock time.
    /// Ties are broken by `rowid`, SQLite's insertion sequence, which is the
    /// honest tie-break for a log that is only ever appended to.
    ///
    /// The previous tie-break was `id`, and that was wrong: an action id embeds
    /// the round's verification counter unpadded, so at equal timestamps
    /// `ACT-VER-AUTH-42-10-0` sorted *before* `ACT-VER-AUTH-42-2-0` — round ten
    /// ahead of round two. The same fault appeared inside a single round once a
    /// task ran eleven or more checks. Neither is a lexical accident that can be
    /// patched by reformatting ids, because the counter is not the only thing in
    /// play and the id must stay stable.
    ///
    /// `rowid` is trustworthy here specifically because this table is append-only:
    /// there is no `DELETE` or `UPDATE` against `task_actions` anywhere in the
    /// crate, so a rowid is never reused or rewritten, and it is stored in the
    /// database file, so the order is identical before and after a reopen.
    ///
    /// What this does *not* claim: that `at` was assigned in commit order. A
    /// caller that backfills an action carrying an older `at` will still see it
    /// sorted by that `at` first, which is correct. At equal `at`, rowid reports
    /// insertion order and nothing more — for a manual or legacy row that is the
    /// order it was written, not an independently established time.
    async fn actions_for_task(&self, task: &TaskId) -> Result<Vec<Action>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {ACTION_COLUMNS} FROM task_actions WHERE task_id = ?1 ORDER BY at, rowid"
            ))
            .map_err(translate_error)?;
        let rows = stmt
            .query_map([task.as_str()], row_to_action)
            .map_err(translate_error)?;
        let mut all = Vec::new();
        for row in rows {
            all.push(row.map_err(translate_error)?);
        }
        Ok(all)
    }
}

/// Insert one action on the caller's transaction.
///
/// Exists so the write can join a larger transaction rather than always opening
/// its own: [`SqliteActionRepository::record_action`] uses it for a standalone
/// write, and the verification path uses it so that an action lands in the *same*
/// transaction as the verdict that observed it. Sharing one function means the
/// two paths cannot drift on columns or encoding.
pub(crate) fn insert_action(
    tx: &rusqlite::Transaction<'_>,
    action: &Action,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO task_actions
            (id, task_id, agent_id, session_id, kind, summary, detail, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            action.id.as_str(),
            action.task_id.as_ref().map(|id| id.to_string()),
            action.agent_id.as_ref().map(|id| id.to_string()),
            action.session_id.as_ref().map(|id| id.to_string()),
            json::to_json(&action.kind)?,
            action.summary,
            action.detail,
            json::timestamp(action.at),
        ],
    )
    .map_err(translate_error)?;
    Ok(())
}

fn load_action(conn: &PooledConn, id: &ActionId) -> Result<Action, StoreError> {
    conn.query_row(
        &format!("SELECT {ACTION_COLUMNS} FROM task_actions WHERE id = ?1"),
        [id.as_str()],
        row_to_action,
    )
    .map_err(translate_error)
}

fn row_to_action(row: &rusqlite::Row<'_>) -> rusqlite::Result<Action> {
    let task_id: Option<String> = row.get(1)?;
    let agent_id: Option<String> = row.get(2)?;
    let session_id: Option<String> = row.get(3)?;
    let kind: String = row.get(4)?;
    let summary: String = row.get(5)?;
    let detail: Option<String> = row.get(6)?;
    let at: String = row.get(7)?;

    Ok(Action {
        id: ActionId::from_string(row.get::<_, String>(0)?),
        task_id: task_id.map(TaskId::from_string),
        agent_id: agent_id.map(AgentId::from_string),
        session_id: session_id.map(SessionId::from_string),
        kind: json::from_json(&kind).map_err(|err| sqlite_conv_failure(4, err))?,
        summary,
        detail,
        at: json::parse_timestamp(&at).map_err(|err| sqlite_conv_failure(7, err))?,
    })
}

fn sqlite_conv_failure(column: usize, err: StoreError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(err))
}
