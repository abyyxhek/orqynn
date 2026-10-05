//! The migration system: how a Orqyn database is brought up to date.
//!
//! ## The rule it enforces
//!
//! > The application never issues DDL at runtime. The schema is a set of
//! > ordered, immutable migration files, and `schema_migrations` is the record
//! > of which ones have been applied.
//!
//! A database is brought up to date by applying every migration whose number is
//! not yet recorded, in order, each in its own transaction. A failure aborts
//! the offending migration and leaves the database at the last good state —
//! never half-way through one.
//!
//! ## Why ordered migrations and not a schema dump
//!
//! A dump tells you what the schema *is*; it does not tell you how to get there
//! from the version a running Orqyn already has. Migrations are the
//! difference between "upgrade works" and "upgrade requires a fresh database".
//! A Orqyn process may open a database written by an older release, and the
//! schema version it finds there is a fact it must honor, not an inconvenience.
//!
//! ## How developers initialize a fresh database
//!
//! ```text
//! // In Rust: the store opens, sees no schema_migrations table, and applies
//! // every migration from 0001 up.
//! let store = Store::open(path).await?;
//! ```
//!
//! Or, to inspect the intended shape by hand:
//!
//! ```sh
//! # Apply the schema to a throwaway database.
//! sqlite3 director.db < crates/director-store/migrations/0001_initial.sql
//! ```
//!
//! Either path produces the same schema. The migration files are the source of
//! truth for both.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension};

use director_domain::StoreError;

/// A migration, with its number and its SQL. Built at compile time from the
/// files in `migrations/`, so a release cannot forget to ship one.
#[derive(Debug, Clone)]
pub struct Migration {
    /// The ordering key, from the filename's leading digits.
    pub version: u32,
    /// A human label for diagnostics.
    pub name: &'static str,
    /// The SQL to apply.
    pub sql: &'static str,
}

/// Every migration known to this build, in version order.
///
/// The `include_str!` calls embed the files at compile time; a migration that
/// is not listed here does not exist, and a file that is not on disk fails the
/// build. Adding a migration is: write the file, add one line here.
pub fn migrations() -> BTreeMap<u32, Migration> {
    let mut all = BTreeMap::new();
    all.insert(
        1,
        Migration {
            version: 1,
            name: "initial",
            sql: include_str!("../migrations/0001_initial.sql"),
        },
    );
    all.insert(
        2,
        Migration {
            version: 2,
            name: "plans_decisions",
            sql: include_str!("../migrations/0002_plans_decisions.sql"),
        },
    );
    all.insert(
        3,
        Migration {
            version: 3,
            name: "verifications",
            sql: include_str!("../migrations/0003_verifications.sql"),
        },
    );
    all
}

/// The highest migration this build can apply. A database past this point was
/// written by a newer Orqyn and is refused rather than silently downgraded.
pub fn latest_version() -> u32 {
    *migrations().keys().max().unwrap_or(&0)
}

/// Create the tracking table if it does not exist. Idempotent — running it on
/// an up-to-date database is a no-op, which is what makes `open` safe to call
/// every time Orqyn starts.
pub(crate) fn ensure_schema_table(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version     INTEGER PRIMARY KEY,
            applied_at  TEXT NOT NULL
        );",
    )
    .map_err(migration_error)?;
    Ok(())
}

/// Read the set of already-applied migration versions.
pub(crate) fn applied_versions(conn: &Connection) -> Result<Vec<u32>, StoreError> {
    let mut stmt = conn
        .prepare("SELECT version FROM schema_migrations ORDER BY version")
        .map_err(migration_error)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, i64>(0).map(|v| v as u32))
        .map_err(migration_error)?;
    let mut versions = Vec::new();
    for row in rows {
        versions.push(row.map_err(migration_error)?);
    }
    Ok(versions)
}

/// Apply every migration not yet recorded, in order, each in its own
/// transaction.
///
/// Returns the number of migrations applied, so an `open` that does nothing can
/// be distinguished from one that upgraded a database — which matters when
/// deciding whether to log "database ready" or "database upgraded".
pub(crate) fn run_migrations(conn: &mut Connection) -> Result<usize, StoreError> {
    ensure_schema_table(conn)?;
    let applied = applied_versions(conn)?;
    let known = migrations();

    // A database that is newer than this build. Refuse: applying nothing would
    // leave Orqyn running against a schema it does not understand, and
    // "downgrade" is not a supported operation.
    if let Some(&newest) = applied.iter().max() {
        if newest > latest_version() {
            return Err(StoreError::Migration(format!(
                "database is at migration {newest}, but this build only knows up to {}. \
                 A newer Orqyn wrote this database; refusing to run against an unknown schema.",
                latest_version()
            )));
        }
    }

    let mut applied_now = 0usize;
    for (version, migration) in &known {
        if applied.contains(version) {
            continue;
        }
        apply_one(conn, migration)?;
        applied_now += 1;
    }
    Ok(applied_now)
}

/// Apply a single migration transactionally. A failure rolls the migration back
/// and surfaces the error; the tracking row is written only after the SQL
/// succeeded, so a recorded migration is always a complete one.
fn apply_one(conn: &mut Connection, migration: &Migration) -> Result<(), StoreError> {
    let tx = conn.transaction().map_err(transaction_error)?;

    tx.execute_batch(migration.sql)
        .map_err(|err| StoreError::Migration(format!("{}: {err}", migration.name)))?;

    let now = chrono::Utc::now().to_rfc3339();
    tx.execute(
        "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
        rusqlite::params![migration.version as i64, now],
    )
    .map_err(migration_error)?;

    tx.commit().map_err(transaction_error)?;
    Ok(())
}

/// The version a database is at, or `None` if it has no schema at all — the
/// signal that `open` is looking at a path that is not a Orqyn database.
pub(crate) fn current_version(conn: &Connection) -> Result<Option<u32>, StoreError> {
    let table_exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(migration_error)?;
    if table_exists.is_none() {
        return Ok(None);
    }
    let version: Option<i64> = conn
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .optional()
        .map_err(migration_error)?;
    Ok(version.map(|v| v as u32))
}

fn migration_error(err: rusqlite::Error) -> StoreError {
    StoreError::Migration(err.to_string())
}

fn transaction_error(err: rusqlite::Error) -> StoreError {
    StoreError::Transaction(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Connection {
        Connection::open_in_memory().expect("in-memory db")
    }

    #[test]
    fn the_migrations_are_registered_contiguously_from_one() {
        let all = migrations();
        assert!(all.contains_key(&1), "migration 1 is the base schema");
        // Versions are unique and the map keeps them ordered. The runner
        // applies by version number with no gaps, so every migration from 1 to
        // the newest must be registered here: a gap would mean a migration file
        // that no build knows about, and an upgrade that silently stops halfway.
        let versions: Vec<u32> = all.keys().copied().collect();
        assert_eq!(
            versions,
            (1..=latest_version()).collect::<Vec<u32>>(),
            "migrations must be contiguous from 1 to the newest"
        );
    }

    #[test]
    fn a_fresh_database_reports_no_schema() {
        let conn = memory();
        assert_eq!(current_version(&conn).unwrap(), None);
    }

    #[test]
    fn running_migrations_on_a_fresh_database_reaches_the_newest_version() {
        let newest = latest_version();
        let mut conn = memory();
        let applied = run_migrations(&mut conn).expect("migrations apply");
        assert_eq!(applied, newest as usize, "every migration is applied");
        assert_eq!(current_version(&conn).unwrap(), Some(newest));
        // The tracking table records one row per migration, so a re-open reads
        // the same count back.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, migrations().len() as i64);
    }

    #[test]
    fn re_running_migrations_is_a_no_op() {
        // The idempotency that makes `open` safe on every start.
        let mut conn = memory();
        run_migrations(&mut conn).unwrap();
        let second = run_migrations(&mut conn).expect("re-run is a no-op");
        assert_eq!(second, 0);
        assert_eq!(current_version(&conn).unwrap(), Some(latest_version()));
    }

    #[test]
    fn the_tables_the_repositories_need_exist_after_migration() {
        let mut conn = memory();
        run_migrations(&mut conn).unwrap();
        for table in [
            "projects",
            "repositories",
            "tasks",
            "task_dependencies",
            "task_status_history",
            "agents",
            "agent_sessions",
            "agent_assignments",
            "checkpoints",
            "plans",
            "decisions",
            "project_states",
            "provider_sync",
        ] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "table {table} should exist");
        }
    }

    #[test]
    fn the_assignment_uniqueness_rule_is_enforced() {
        // The partial unique index: two active assignments for one task must
        // fail, because that is the "at most one active assignment" invariant.
        //
        // Status is written the way the repository writes it — as JSON text, so
        // a snake_case variant is the quoted string "active". Testing in that
        // form is what makes this a check of the rule the store actually
        // relies on: a bare 'active' would exercise a different value than any
        // repository call ever stores.
        let mut conn = memory();
        run_migrations(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO projects (id, name, root, default_branch, created_at, updated_at)
             VALUES ('PROJ-1', 'p', '/repo', 'main', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, harness, machine, capabilities, status,
                                state_version, registered_at, last_seen)
             VALUES ('AGENT-1', 'a', 'claude_code', 'MACH-a', '[]', 'available',
                     1, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tasks (id, project_id, title, objective, status, complexity,
                                expected_outputs, dependencies, scope_paths,
                                required_capabilities, subtasks, created_at, updated_at)
             VALUES ('AUTH-42', 'PROJ-1', 't', 'o', 'todo', 'unknown',
                     '[]', '[]', '[]', '[]', '[]',
                     '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agent_assignments (id, task_id, agent_id, status, assigned_at,
                                            state_version)
             VALUES ('ASG-1', 'AUTH-42', 'AGENT-1', '\"active\"',
                     '2026-01-01T00:00:00Z', 1)",
            [],
        )
        .unwrap();
        let second = conn.execute(
            "INSERT INTO agent_assignments (id, task_id, agent_id, status, assigned_at,
                                            state_version)
             VALUES ('ASG-2', 'AUTH-42', 'AGENT-1', '\"active\"',
                     '2026-01-01T00:00:00Z', 1)",
            [],
        );
        assert!(
            second.is_err(),
            "a second active assignment must be rejected"
        );

        // But a released assignment alongside an active one is fine — history
        // is retained, and the released one does not occupy the slot.
        conn.execute(
            "INSERT INTO agent_assignments (id, task_id, agent_id, status, assigned_at,
                                            state_version)
             VALUES ('ASG-3', 'AUTH-42', 'AGENT-1', '\"released\"',
                     '2026-01-01T00:00:00Z', 1)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn the_checkpoint_uniqueness_rule_is_enforced() {
        // Only one current checkpoint per task; superseded ones coexist.
        let mut conn = memory();
        run_migrations(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO projects (id, name, root, default_branch, created_at, updated_at)
             VALUES ('PROJ-1', 'p', '/repo', 'main', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tasks (id, project_id, title, objective, status, complexity,
                                expected_outputs, dependencies, scope_paths,
                                required_capabilities, subtasks, created_at, updated_at)
             VALUES ('AUTH-42', 'PROJ-1', 't', 'o', 'todo', 'unknown',
                     '[]', '[]', '[]', '[]', '[]',
                     '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO checkpoints (id, task_id, objective, progress, progress_fraction,
                                      changed_files, important_decisions, current_assumptions,
                                      next_action, context_version, status, created_at,
                                      state_version)
             VALUES ('CHK-1', 'AUTH-42', 'o', 'p', 0.0, '[]', '[]', '[]', 'n',
                     1, '\"current\"', '2026-01-01T00:00:00Z', 1)",
            [],
        )
        .unwrap();
        let second = conn.execute(
            "INSERT INTO checkpoints (id, task_id, objective, progress, progress_fraction,
                                      changed_files, important_decisions, current_assumptions,
                                      next_action, context_version, status, created_at,
                                      state_version)
             VALUES ('CHK-2', 'AUTH-42', 'o', 'p', 0.0, '[]', '[]', '[]', 'n',
                     1, '\"current\"', '2026-01-01T00:00:00Z', 1)",
            [],
        );
        assert!(
            second.is_err(),
            "a second current checkpoint must be rejected"
        );

        // A superseded one alongside the current one is exactly the intended shape.
        conn.execute(
            "INSERT INTO checkpoints (id, task_id, objective, progress, progress_fraction,
                                      changed_files, important_decisions, current_assumptions,
                                      next_action, context_version, status, created_at,
                                      state_version)
             VALUES ('CHK-3', 'AUTH-42', 'o', 'p', 0.0, '[]', '[]', '[]', 'n',
                     1, '\"superseded\"', '2026-01-01T00:00:00Z', 1)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn a_failing_migration_leaves_no_tracking_row() {
        // A migration that fails partway must not be recorded as applied.
        let mut conn = memory();
        ensure_schema_table(&conn).unwrap();
        let bad = Migration {
            version: 99,
            name: "bad",
            sql: "CREATE TABLE broken (x INTEGER); SELECT * FROM table_that_does_not_exist;",
        };
        assert!(apply_one(&mut conn, &bad).is_err());
        let applied = applied_versions(&conn).unwrap();
        assert!(!applied.contains(&99), "a failed migration is not recorded");
        // And the part that did parse was rolled back with it.
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'broken'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 0, "the failed migration's table was rolled back");
    }

    #[test]
    fn a_database_newer_than_this_build_is_refused() {
        let mut conn = memory();
        ensure_schema_table(&conn).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (999, '2026-01-01')",
            [],
        )
        .unwrap();
        let err = run_migrations(&mut conn).unwrap_err();
        assert!(matches!(err, StoreError::Migration(_)));
        let msg = format!("{err}");
        assert!(msg.contains("999"));
    }
}
