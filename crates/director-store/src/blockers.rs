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
use director_domain::ids::{AgentId, BlockerId, Id, TaskId};
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

    /// Raise a blocker against `task`, allocating its id here rather than taking
    /// one from the caller.
    ///
    /// [`BlockerRepository::create_blocker`] takes the id as part of the record,
    /// which is right for a caller that has one and wrong for a caller that would
    /// have to *choose* one. A caller choosing an id has to count the task's
    /// existing blockers to pick a free ordinal, and that count read outside a
    /// write transaction is a guess: two callers raising blockers against the same
    /// task concurrently can read the same length, build the same
    /// `BLK-{task}-{ordinal}`, and have the second one refused by the primary key
    /// on `blockers.id`. Nothing about either blocker was wrong; only the names
    /// collided.
    ///
    /// So the allocation happens here, in one transaction that takes the write lock
    /// *before* it reads, counts the task's blockers, constructs the id, and
    /// inserts the row before releasing it. A competing caller cannot interleave:
    /// it would need the same reserved write lock, which is held until this
    /// transaction commits or rolls back. The count the ordinal comes from
    /// therefore includes every blocker committed before this one and none
    /// committed after it.
    ///
    /// The count is over *all* of the task's rows, cleared ones included, because
    /// blockers are never deleted — so the ordinal is monotonic and an id freed by
    /// a `Resolved` or `Obsolete` transition is never handed to a different
    /// blocker.
    ///
    /// This is additive. [`BlockerRepository::create_blocker`] keeps its
    /// signature and its caller-chosen-id behaviour, and the domain trait is
    /// unchanged: allocation is a store concern, so it is offered as an inherent
    /// method on the SQLite repository rather than as a new trait obligation every
    /// implementation would have to satisfy.
    pub async fn create_blocker_next(
        &self,
        task_id: &TaskId,
        kind: director_domain::blocker::BlockerKind,
        description: impl Into<String>,
        resolution: Option<String>,
        raised_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<Blocker, StoreError> {
        create_next(
            &self.pool,
            task_id,
            kind,
            description.into(),
            resolution,
            raised_at,
        )
        .await
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
                "SELECT {BLOCKER_COLUMNS} FROM blockers WHERE task_id = ?1 ORDER BY raised_at, id"
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

/// Raise a blocker against `task`, allocating the id inside the write
/// transaction.
///
/// The transaction is `IMMEDIATE` rather than deferred, and that is the whole
/// point. A deferred transaction does not take the write lock until its first
/// *write*, so its earlier reads come from a snapshot taken before any competing
/// writer committed — exactly the window this operation exists to close. Taking
/// the reserved write lock up front means the `COUNT` and the `INSERT` are
/// serialized against every other writer, not merely adjacent.
///
/// Nothing else is written: a raised blocker is one row, and its status stays
/// `Open`. This deliberately does not touch the task's status — a blocker is its
/// own lifecycle, and the task's is the loop's to decide.
pub async fn create_next(
    pool: &crate::connection::ConnectionPool,
    task_id: &TaskId,
    kind: director_domain::blocker::BlockerKind,
    description: String,
    resolution: Option<String>,
    raised_at: chrono::DateTime<chrono::Utc>,
) -> Result<Blocker, StoreError> {
    let mut conn = pool.get();
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(translate_error)?;

    let blocker = next_blocker(&tx, task_id, kind, description, resolution, raised_at)?;
    insert_blocker(&tx, &blocker)?;
    tx.commit().map_err(translate_error)?;
    drop(conn);

    let conn = pool.get();
    load_blocker(&conn, &blocker.id)
}

/// Build the next blocker for `task`, counting inside `tx`.
///
/// Every row for the task counts, cleared ones included, because blockers are
/// never deleted. That is what makes the ordinal monotonic and stops a resolved
/// blocker's id from being handed out again.
fn next_blocker(
    tx: &rusqlite::Transaction<'_>,
    task_id: &TaskId,
    kind: director_domain::blocker::BlockerKind,
    description: String,
    resolution: Option<String>,
    raised_at: chrono::DateTime<chrono::Utc>,
) -> Result<Blocker, StoreError> {
    let prior: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM blockers WHERE task_id = ?1",
            [task_id.as_str()],
            |row| row.get(0),
        )
        .map_err(translate_error)?;

    let mut blocker = director_domain::blocker::Blocker::raise(
        BlockerId::from_string(format!("{}-{task_id}-{}", BlockerId::PREFIX, prior + 1)),
        task_id.clone(),
        kind,
        description,
    );
    // `raised_at` is the caller's instant rather than the constructor's wall
    // clock, so a caller can pin it.
    blocker.raised_at = raised_at;
    if let Some(resolution) = resolution {
        blocker = blocker.resolvable_by(resolution);
    }
    Ok(blocker)
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
