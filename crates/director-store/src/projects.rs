//! [`ProjectRepository`] over SQLite.
//!
//! Projects are the root everything else hangs off, and most of this file is
//! the mapping between a [`Project`] and its row. The interesting behavior is
//! the optimistic version check on update — see [`update_project`] — and the
//! rejection of a duplicate id on create.

use async_trait::async_trait;

use director_domain::ids::{ProjectId, RepositoryId};
use director_domain::project::Project;
use director_domain::repository::Repository;
use director_domain::StoreError;

use crate::connection::{translate_error, PooledConn};
use crate::json;

/// The store-facing implementation of [`ProjectRepository`].
#[derive(Clone)]
pub struct SqliteProjectRepository {
    pool: crate::connection::ConnectionPool,
}

impl SqliteProjectRepository {
    pub(crate) fn new(pool: crate::connection::ConnectionPool) -> Self {
        SqliteProjectRepository { pool }
    }

    fn conn(&self) -> PooledConn {
        self.pool.get()
    }
}

#[async_trait]
impl director_domain::Store for SqliteProjectRepository {
    type Error = StoreError;
}

#[async_trait]
impl director_domain::ProjectRepository for SqliteProjectRepository {
    async fn create_project(&self, project: &Project) -> Result<Project, StoreError> {
        let project = project.clone();
        let conn = self.conn();
        conn.execute(
            "INSERT INTO projects (id, name, root, default_branch, repository_id,
                                   state_version, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                project.id.as_str(),
                project.name,
                project.root,
                json::to_json(&project.default_branch)?,
                project.repository_id.as_ref().map(RepositoryId::as_str),
                project.state_version as i64,
                json::timestamp(project.created_at),
                json::timestamp(project.updated_at),
            ],
        )
        .map_err(translate_error)?;

        // The record as stored, read back — the caller sees exactly what the
        // store holds, not what it was handed.
        load_project(&conn, &project.id)
    }

    async fn get_project(&self, id: &ProjectId) -> Result<Project, StoreError> {
        let conn = self.conn();
        load_project(&conn, id)
    }

    async fn update_project(&self, project: &Project) -> Result<Project, StoreError> {
        let project = project.clone();
        let conn = self.conn();
        update_project(&conn, &project)?;
        load_project(&conn, &project.id)
    }

    async fn list_projects(&self) -> Result<Vec<Project>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, root, default_branch, repository_id, state_version,
                        created_at, updated_at
                 FROM projects ORDER BY name",
            )
            .map_err(translate_error)?;
        let rows = stmt
            .query_map([], row_to_project)
            .map_err(translate_error)?;
        let mut all = Vec::new();
        for row in rows {
            all.push(row.map_err(translate_error)?);
        }
        Ok(all)
    }
}

/// Write an updated project with its optimistic version check.
///
/// The `WHERE state_version = ?` clause is the whole concurrency story: if
/// another writer moved the version on, this matches zero rows and the caller
/// gets a [`StoreError::StateVersionConflict`] instead of a silent overwrite.
pub(crate) fn update_project(conn: &PooledConn, project: &Project) -> Result<(), StoreError> {
    let rows = conn
        .execute(
            "UPDATE projects
                SET name = ?2, root = ?3, default_branch = ?4, repository_id = ?5,
                    state_version = state_version + 1, updated_at = ?6
              WHERE id = ?1 AND state_version = ?7",
            rusqlite::params![
                project.id.as_str(),
                project.name,
                project.root,
                json::to_json(&project.default_branch)?,
                project.repository_id.as_ref().map(RepositoryId::as_str),
                json::timestamp(chrono::Utc::now()),
                project.state_version as i64,
            ],
        )
        .map_err(translate_error)?;
    crate::connection::row_count_to_outcome(
        rows,
        "project",
        project.id.as_str(),
        project.state_version,
    )
}

/// Load one project by id, or [`StoreError::NotFound`].
pub(crate) fn load_project(conn: &PooledConn, id: &ProjectId) -> Result<Project, StoreError> {
    conn.query_row(
        "SELECT id, name, root, default_branch, repository_id, state_version,
                created_at, updated_at
         FROM projects WHERE id = ?1",
        [id.as_str()],
        row_to_project,
    )
    .map_err(translate_error)
}

/// Map a row into a [`Project`]. A bad column is an error, not a default: a
/// timestamp that does not parse means the stored value and this code disagree.
fn row_to_project(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    let default_branch_text: String = row.get(3)?;
    let repository_id: Option<String> = row.get(4)?;
    let created_at: String = row.get(6)?;
    let updated_at: String = row.get(7)?;
    let created = json::parse_timestamp(&created_at).map_err(sqlite_conv_failure(6))?;
    let updated = json::parse_timestamp(&updated_at).map_err(sqlite_conv_failure(7))?;
    let branch = json::from_json(&default_branch_text).map_err(sqlite_conv_failure(3))?;
    Ok(Project {
        id: ProjectId::from_string(row.get::<_, String>(0)?),
        name: row.get(1)?,
        root: row.get(2)?,
        default_branch: branch,
        repository_id: repository_id.map(RepositoryId::from_string),
        state_version: row.get::<_, i64>(5)? as u64,
        created_at: created,
        updated_at: updated,
    })
}

/// Lift a store error into the rusqlite shape a row mapper must return, pinning
/// the column index so the message says which field is at fault.
fn sqlite_conv_failure(
    index: usize,
) -> impl Fn(director_domain::StoreError) -> rusqlite::Error + 'static {
    move |err: director_domain::StoreError| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, err.into())
    }
}

/// Register a repository record — the *link* Orqyn keeps, distinct from the
/// git observer's own detailed record of the same repository.
pub async fn register_repository(
    pool: &crate::connection::ConnectionPool,
    repository: &Repository,
) -> Result<(), StoreError> {
    let conn = pool.get();
    conn.execute(
        "INSERT INTO repositories (id, project_id, local_path, remote_url, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET
             project_id = excluded.project_id,
             local_path = excluded.local_path,
             remote_url = excluded.remote_url",
        rusqlite::params![
            repository.id.as_str(),
            repository.project_id.as_str(),
            repository.local_path,
            repository.remote_url,
            json::timestamp(repository.created_at),
        ],
    )
    .map_err(translate_error)?;
    Ok(())
}

/// Every repository Orqyn knows about, as the lightweight link records the
/// store keeps.
pub async fn list_repositories(
    pool: &crate::connection::ConnectionPool,
) -> Result<Vec<RepositoryId>, StoreError> {
    let conn = pool.get();
    let mut stmt = conn
        .prepare("SELECT id FROM repositories ORDER BY id")
        .map_err(translate_error)?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(translate_error)?;
    let mut all = Vec::new();
    for row in rows {
        all.push(RepositoryId::from_string(row.map_err(translate_error)?));
    }
    Ok(all)
}
