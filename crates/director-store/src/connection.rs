//! Connection setup: the PRAGMAs, the pool, and error translation.
//!
//! ## Why each reliability setting exists
//!
//! SQLite's defaults are chosen for a single-process, single-connection,
//! serverless embedded case. Orqyn is a long-running multi-task process with
//! several repositories hitting one database, so three defaults are wrong and
//! each setting below corrects one specific thing rather than being cargo-culted:
//!
//! - **`journal_mode = WAL`.** In the default rollback-journal mode a writer
//!   takes an exclusive lock that blocks every reader for the whole write. WAL
//!   lets readers proceed while a write commits, which matters because Orqyn
//!   observes, plans, and assigns concurrently rather than serializing the
//!   whole loop per write. WAL is a persistent property of the *file*, not the
//!   connection, so setting it once is enough — but setting it again is free.
//! - **`foreign_keys = ON`.** SQLite **ignores** foreign keys by default for
//!   backwards compatibility with pre-2004 databases. Without this the
//!   `REFERENCES` declarations in the schema are documentation only, and an
//!   assignment for a task that does not exist would be stored. This one is
//!   per-connection and must be set on every connection, which is why it lives
//!   here rather than in a migration.
//! - **`synchronous = NORMAL`.** With WAL, `NORMAL` does not fsync on every
//!   commit — only on checkpoint. That is a real durability trade, made
//!   deliberately: Orqyn's acceptance property is surviving a *process*
//!   restart, not a power pull that takes the whole machine with unflushed
//!   kernel buffers. The throughput win on every write is worth that line, and
//!   it is the setting SQLite's own documentation recommends alongside WAL.
//! - **`busy_timeout`.** Two writers can still contend on the write lock even
//!   under WAL. The default is to fail immediately with `SQLITE_BUSY`, which
//!   turns ordinary contention into spurious errors. A timeout makes the loser
//!   wait instead, which is what makes concurrent writers behave deterministically.
//!
//! ## Connection lifecycle
//!
//! The store owns a small pool. rusqlite's `Connection` is not `Sync`, so it
//! cannot sit behind a plain `&self` while several tasks call in; the pool
//! hands out a connection for the duration of one operation and takes it back.
//! A connection is held across one repository call and never across an await
//! point, so the pool is sized for the concurrency Orqyn actually does.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags};

use director_domain::StoreError;

use crate::migrations;

/// How long a writer waits for the write lock before reporting contention.
///
/// Chosen to be comfortably longer than a migration or a batch write, and short
/// enough that a genuinely stuck database surfaces rather than hanging the loop.
const BUSY_TIMEOUT_MS: u64 = 5000;

/// The maximum number of concurrent connections. Repository methods are short
/// synchronous units, so this bounds concurrency rather than queuing depth.
const POOL_SIZE: usize = 8;

/// A connection borrowed from the pool. Returns itself on drop, so the borrow
/// is bounded by the call that took it.
pub(crate) struct PooledConn {
    conn: Option<Connection>,
    pool: Arc<ConnectionPoolInner>,
}

impl std::ops::Deref for PooledConn {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("connection returned twice")
    }
}

impl std::ops::DerefMut for PooledConn {
    fn deref_mut(&mut self) -> &mut Connection {
        self.conn.as_mut().expect("connection returned twice")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.return_conn(conn);
        }
    }
}

struct ConnectionPoolInner {
    conns: Mutex<Vec<Connection>>,
}

impl ConnectionPoolInner {
    /// Return a connection to the pool. Called from `PooledConn::drop`, so a
    /// connection that leaves the pool always comes back.
    fn return_conn(&self, conn: Connection) {
        self.conns.lock().expect("pool mutex poisoned").push(conn);
    }
}

/// A fixed-size pool of connections, created once when the store opens.
///
/// Cheaply cloneable (it is an `Arc` underneath) so every repository can hold
/// one without owning the store. The last clone dropped tears the pool down.
#[derive(Clone)]
pub(crate) struct ConnectionPool {
    inner: Arc<ConnectionPoolInner>,
}

impl ConnectionPool {
    /// Build a pool of `POOL_SIZE` connections to `path`, each configured and
    /// migrated. Every connection runs the migration: it is idempotent, and
    /// doing it on all of them means the first caller through any connection
    /// sees a ready schema.
    pub(crate) fn open(path: &Path) -> Result<Self, StoreError> {
        let mut conns = Vec::with_capacity(POOL_SIZE);
        for _ in 0..POOL_SIZE {
            conns.push(open_and_configure(path)?);
        }
        Ok(ConnectionPool {
            inner: Arc::new(ConnectionPoolInner {
                conns: Mutex::new(conns),
            }),
        })
    }

    /// Take a connection, panicking if all are out. That is deliberate: an
    /// exhausted pool means a caller is holding a connection across an await
    /// point, which is a bug worth stopping for rather than silently queuing on.
    pub(crate) fn get(&self) -> PooledConn {
        let conn = self
            .inner
            .conns
            .lock()
            .expect("pool mutex poisoned")
            .pop()
            .expect("connection pool exhausted; a connection is held across an await?");
        // Re-assert the per-connection settings. They persist on the
        // connection, but re-asserting keeps the invariant independent of any
        // future code path that resets them.
        set_pragmas(&conn).expect("PRAGMAs must apply to a pooled connection");
        PooledConn {
            conn: Some(conn),
            pool: Arc::clone(&self.inner),
        }
    }
}

/// Open one connection and configure it, bringing the schema up to date.
pub(crate) fn open_and_configure(path: &Path) -> Result<Connection, StoreError> {
    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|err| StoreError::Storage(format!("could not open {}: {err}", path.display())))?;

    set_pragmas(&conn)?;

    let applied = migrations::run_migrations(&mut conn)?;
    if applied > 0 {
        tracing::info!(applied, "director store: schema migrated");
    }
    Ok(conn)
}

/// Apply the reliability settings to a connection. See the module docs for why
/// each one is what it is.
pub(crate) fn set_pragmas(conn: &Connection) -> Result<(), StoreError> {
    // journal_mode is a file-level property; the returned value is the mode
    // SQLite actually settled on, so WAL is confirmed rather than assumed —
    // some filesystems cannot do WAL and silently fall back.
    let mode: String = conn
        .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
        .map_err(|err| StoreError::Storage(format!("set journal_mode: {err}")))?;
    if mode != "wal" {
        return Err(StoreError::Storage(format!(
            "journal_mode is {mode}, expected wal; the database may be on a filesystem \
             that does not support WAL"
        )));
    }

    // Per-connection: without this the schema's REFERENCES clauses are inert.
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|err| StoreError::Storage(format!("set foreign_keys: {err}")))?;

    // Deliberate durability trade, documented at the top of this module.
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|err| StoreError::Storage(format!("set synchronous: {err}")))?;

    // Contention waits instead of erroring.
    conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))
        .map_err(|err| StoreError::Storage(format!("set busy_timeout: {err}")))?;

    Ok(())
}

/// Translate a rusqlite error into Orqyn's vocabulary, so no caller ever has
/// to know it is talking to SQLite.
pub(crate) fn translate_error(err: rusqlite::Error) -> StoreError {
    use rusqlite::Error as E;
    match err {
        E::QueryReturnedNoRows => StoreError::NotFound("no row matched the id".into()),
        E::SqliteFailure(ref fail, ref msg) => {
            // The extended code names the constraint that fired, which is what
            // distinguishes a duplicate key from a dangling reference.
            let detail = msg.as_deref().unwrap_or("no detail from sqlite");
            StoreError::ConstraintViolation(format!(
                "sqlite error {}: {detail}",
                fail.extended_code
            ))
        }
        // A conversion failure is a shape mismatch between a column and a
        // domain type, not a database fault — the JSON layout drifted.
        E::ToSqlConversionFailure(_) | E::FromSqlConversionFailure(_, _, _) => {
            StoreError::Serialization(err.to_string())
        }
        _ => StoreError::Storage(err.to_string()),
    }
}

/// Interpret the row count of a versioned update. Zero rows means the caller's
/// version is no longer current; that is the conflict, never a silent no-op.
pub(crate) fn row_count_to_outcome(
    rows: usize,
    entity: &'static str,
    id: &str,
    caller_version: u64,
) -> Result<(), StoreError> {
    if rows == 0 {
        Err(StoreError::StateVersionConflict {
            entity,
            id: id.to_string(),
            // The stored version is at least caller + 1. The caller re-reads to
            // discover the real current value; reporting a precise one here
            // would require a second query on every conflict.
            stored: caller_version + 1,
            caller: caller_version,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("director-store-conn-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn a_pool_hands_out_and_reuses_connections() {
        let path = temp_path("pool.db");
        let pool = ConnectionPool::open(&path).unwrap();
        let guard = pool.get();
        let mode: String = guard
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        drop(guard);
        // Returned, and available again.
        let _again = pool.get();
    }

    #[test]
    fn foreign_keys_are_actually_enforced_on_a_connection() {
        // The setting must be real, not just declared in the schema.
        let path = temp_path("fk.db");
        let pool = ConnectionPool::open(&path).unwrap();
        let conn = pool.get();
        let on: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(on, 1);
        // Inserting a row whose foreign key has no target fails.
        let err = conn.execute(
            "INSERT INTO tasks (id, project_id, title, objective, status, complexity,
                                expected_outputs, dependencies, scope_paths,
                                required_capabilities, subtasks, created_at, updated_at)
             VALUES ('AUTH-1', 'PROJ-nope', 't', 'o', 'todo', 'unknown',
                     '[]', '[]', '[]', '[]', '[]',
                     '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [],
        );
        assert!(err.is_err(), "an orphan task must be rejected");
    }

    #[test]
    fn synchronous_is_normal_not_full() {
        // The durability trade, confirmed rather than assumed.
        let path = temp_path("sync.db");
        let pool = ConnectionPool::open(&path).unwrap();
        let conn = pool.get();
        // SQLite reports synchronous as an integer, not a name: 0=OFF, 1=NORMAL,
        // 2=FULL, 3=EXTRA. NORMAL is the value the PRAGMA above asked for.
        let level: i64 = conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(level, 1);
    }

    #[test]
    fn a_zero_row_count_is_a_conflict_with_the_callers_version() {
        let err = row_count_to_outcome(0, "task", "AUTH-42", 12).unwrap_err();
        match err {
            StoreError::StateVersionConflict {
                entity,
                id,
                caller,
                stored,
            } => {
                assert_eq!(entity, "task");
                assert_eq!(id, "AUTH-42");
                assert_eq!(caller, 12);
                assert_eq!(stored, 13);
            }
            _ => panic!("expected a conflict"),
        }
        assert!(row_count_to_outcome(1, "task", "AUTH-42", 12).is_ok());
    }

    #[test]
    fn a_missing_row_translates_to_not_found() {
        let err = translate_error(rusqlite::Error::QueryReturnedNoRows);
        assert!(matches!(err, StoreError::NotFound(_)));
    }
}
