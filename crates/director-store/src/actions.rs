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
        tx.commit().map_err(translate_error)?;
        drop(conn);

        let conn = self.conn();
        load_action(&conn, &action.id)
    }

    async fn get_action(&self, id: &ActionId) -> Result<Action, StoreError> {
        let conn = self.conn();
        load_action(&conn, id)
    }

    async fn actions_for_task(&self, task: &TaskId) -> Result<Vec<Action>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {ACTION_COLUMNS} FROM task_actions WHERE task_id = ?1 ORDER BY at, id"
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
