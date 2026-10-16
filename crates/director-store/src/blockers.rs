//! [`BlockerRepository`] over SQLite.
//!
//! A blocker is retained after it stops blocking. [`update_blocker`] writes the
//! new state over the row — status, resolution, `resolved_at` — and never
//! deletes, so "we fixed it" stays distinguishable from "we stopped trying" and
//! a replacement agent can read the history rather than only the current answer.
//!
//! Every column is stored the way the rest of the store stores it: enums and
//! timestamps as JSON text, written and parsed through [`crate::json`], so a
//! malformed value is an error rather than a silent default.

use async_trait::async_trait;
use director_domain::blocker::Blocker;
use director_domain::ids::{AgentId, BlockerId, TaskId};
use director_domain::StoreError;

use crate::connection::{translate_error, PooledConn};
use crate::json;

const BLOCKER_COLUMNS: &str = "id, task_id, kind, status, description, resolution, \
     raised_by, raised_at, resolved_at";

/// The store-facing implementation of [`BlockerRepository`].
#[derive(Clone)]
pub struct SqliteBlockerRepository {
    pool: crate::connection::ConnectionPool,
}

impl SqliteBlockerRepository {
    pub(crate) fn new(pool: crate::connection::ConnectionPool) -> Self {
        SqliteBlockerRepository { pool }
    }

    fn conn(&self) -> PooledConn {
        self.pool.get()
    }
}

#[async_trait]
impl director_domain::Store for SqliteBlockerRepository {
    type Error = StoreError;
}

#[async_trait]
impl director_domain::BlockerRepository for SqliteBlockerRepository {
    async fn create_blocker(&self, blocker: &Blocker) -> Result<Blocker, StoreError> {
        let blocker = blocker.clone();
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(translate_error)?;
        insert_blocker(&tx, &blocker)?;
        tx.commit().map_err(translate_error)?;
        drop(conn);

        let conn = self.conn();
        load_blocker(&conn, &blocker.id)
    }

    async fn get_blocker(&self, id: &BlockerId) -> Result<Blocker, StoreError> {
        let conn = self.conn();
        load_blocker(&conn, id)
    }

    async fn update_blocker(&self, blocker: &Blocker) -> Result<Blocker, StoreError> {
        let blocker = blocker.clone();
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(translate_error)?;
        update_blocker(&tx, &blocker)?;
        tx.commit().map_err(translate_error)?;
        drop(conn);

        let conn = self.conn();
        load_blocker(&conn, &blocker.id)
    }

    async fn blockers_for_task(&self, task: &TaskId) -> Result<Vec<Blocker>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {BLOCKER_COLUMNS} FROM blockers WHERE task_id = ?1 ORDER BY raised_at"
            ))
            .map_err(translate_error)?;
        let rows = stmt
            .query_map([task.as_str()], row_to_blocker)
            .map_err(translate_error)?;
        let mut all = Vec::new();
        for row in rows {
            all.push(row.map_err(translate_error)?);
        }
        Ok(all)
    }
}

fn insert_blocker(tx: &rusqlite::Transaction<'_>, blocker: &Blocker) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO blockers
            (id, task_id, kind, status, description, resolution, raised_by, raised_at, resolved_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            blocker.id.as_str(),
            blocker.task_id.as_str(),
            json::to_json(&blocker.kind)?,
            json::to_json(&blocker.status)?,
            blocker.description,
            blocker.resolution,
            blocker.raised_by.as_ref().map(|id| id.to_string()),
            json::timestamp(blocker.raised_at),
            blocker.resolved_at.map(json::timestamp),
        ],
    )
    .map_err(translate_error)?;
    Ok(())
}

/// Overwrite the mutable columns of an existing row.
///
/// The row is never removed, and `raised_at` is deliberately not among the
/// writable columns: when a blocker was noticed is a fact about the past that a
/// later update must not be able to rewrite.
fn update_blocker(tx: &rusqlite::Transaction<'_>, blocker: &Blocker) -> Result<(), StoreError> {
    let changed = tx
        .execute(
            "UPDATE blockers
                SET kind = ?2, status = ?3, description = ?4, resolution = ?5,
                    raised_by = ?6, resolved_at = ?7
              WHERE id = ?1",
            rusqlite::params![
                blocker.id.as_str(),
                json::to_json(&blocker.kind)?,
                json::to_json(&blocker.status)?,
                blocker.description,
                blocker.resolution,
                blocker.raised_by.as_ref().map(|id| id.to_string()),
                blocker.resolved_at.map(json::timestamp),
            ],
        )
        .map_err(translate_error)?;
    if changed == 0 {
        return Err(StoreError::NotFound(blocker.id.to_string()));
    }
    Ok(())
}

fn load_blocker(conn: &PooledConn, id: &BlockerId) -> Result<Blocker, StoreError> {
    conn.query_row(
        &format!("SELECT {BLOCKER_COLUMNS} FROM blockers WHERE id = ?1"),
        [id.as_str()],
        row_to_blocker,
    )
    .map_err(translate_error)
}

fn row_to_blocker(row: &rusqlite::Row<'_>) -> rusqlite::Result<Blocker> {
    let kind: String = row.get(2)?;
    let status: String = row.get(3)?;
    let description: String = row.get(4)?;
    let resolution: Option<String> = row.get(5)?;
    let raised_by: Option<String> = row.get(6)?;
    let raised_at: String = row.get(7)?;
    let resolved_at: Option<String> = row.get(8)?;

    Ok(Blocker {
        id: BlockerId::from_string(row.get::<_, String>(0)?),
        task_id: TaskId::from_string(row.get::<_, String>(1)?),
        kind: json::from_json(&kind).map_err(|err| sqlite_conv_failure(2, err))?,
        status: json::from_json(&status).map_err(|err| sqlite_conv_failure(3, err))?,
        description,
        resolution,
        raised_by: raised_by.map(AgentId::from_string),
        raised_at: json::parse_timestamp(&raised_at).map_err(|err| sqlite_conv_failure(7, err))?,
        resolved_at: resolved_at
            .map(|text| json::parse_timestamp(&text))
            .transpose()
            .map_err(|err| sqlite_conv_failure(8, err))?,
    })
}

fn sqlite_conv_failure(column: usize, err: StoreError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(err))
}
