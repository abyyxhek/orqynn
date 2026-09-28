//! [`ProjectStateRepository`] and [`ProviderSyncRepository`] over SQLite.
//!
//! ## Project state, and what it is not
//!
//! `project_states` holds the *normalized state Orqyn currently believes a
//! project is in* — one row per project, replaced on each observation. It is
//! deliberately not the git observation itself: the git observer reads `git`
//! fresh and owns the detail of what it saw, while this record is what Orqyn
//! holds between observations, compares a checkpoint against, and survives a
//! restart. The two are separate responsibilities, kept in separate crates.
//!
//! ## Provider sync
//!
//! `provider_sync` is a pointer and a status, never a replica: which provider a
//! Orqyn id corresponds to, when it last synced, and what the last attempt
//! said. The provider's internal schema is not copied, by design — copying it
//! is how a store starts being coupled to a substrate.

use async_trait::async_trait;
use rusqlite::OptionalExtension;

use director_domain::ids::{ProjectId, RepositoryId};
use director_domain::ProviderSync;
use director_domain::{StoreError, StoredProjectState};

use crate::connection::{translate_error, PooledConn};
use crate::json;

/// The store-facing implementation of [`ProjectStateRepository`].
#[derive(Clone)]
pub struct SqliteProjectStateRepository {
    pool: crate::connection::ConnectionPool,
}

impl SqliteProjectStateRepository {
    pub(crate) fn new(pool: crate::connection::ConnectionPool) -> Self {
        SqliteProjectStateRepository { pool }
    }

    fn conn(&self) -> PooledConn {
        self.pool.get()
    }
}

#[async_trait]
impl director_domain::Store for SqliteProjectStateRepository {
    type Error = StoreError;
}

#[async_trait]
impl director_domain::ProjectStateRepository for SqliteProjectStateRepository {
    async fn update_project_state(
        &self,
        state: &StoredProjectState,
    ) -> Result<StoredProjectState, StoreError> {
        let state = state.clone();
        let conn = self.conn();
        // One row per project, replaced wholesale: this record is a belief, and
        // a new observation replaces the belief rather than appending to it.
        conn.execute(
            "INSERT INTO project_states (project_id, repository_id, branch, head_commit,
                                         working_tree_clean, observation_version,
                                         last_observed_at, state_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(project_id) DO UPDATE SET
                 repository_id = excluded.repository_id,
                 branch = excluded.branch,
                 head_commit = excluded.head_commit,
                 working_tree_clean = excluded.working_tree_clean,
                 observation_version = excluded.observation_version,
                 last_observed_at = excluded.last_observed_at,
                 state_version = project_states.state_version + 1",
            rusqlite::params![
                state.project_id.as_str(),
                state.repository_id.as_ref().map(RepositoryId::as_str),
                state.branch,
                state.head_commit,
                state.working_tree_clean as i64,
                state.observation_version as i64,
                json::timestamp(state.last_observed_at),
                state.state_version as i64,
            ],
        )
        .map_err(translate_error)?;
        load_project_state(&conn, &state.project_id)?
            .ok_or_else(|| StoreError::Storage("project state row missing after upsert".into()))
    }

    async fn get_project_state(
        &self,
        project: &ProjectId,
    ) -> Result<Option<StoredProjectState>, StoreError> {
        let conn = self.conn();
        load_project_state(&conn, project)
    }
}

/// Load the state Orqyn holds for a project, or `None` before the first
/// observation.
pub(crate) fn load_project_state(
    conn: &PooledConn,
    project: &ProjectId,
) -> Result<Option<StoredProjectState>, StoreError> {
    conn.query_row(
        "SELECT project_id, repository_id, branch, head_commit, working_tree_clean,
                observation_version, last_observed_at, state_version
         FROM project_states WHERE project_id = ?1",
        [project.as_str()],
        row_to_project_state,
    )
    .optional()
    .map_err(translate_error)
}

fn row_to_project_state(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredProjectState> {
    let repository_id: Option<String> = row.get(1)?;
    let clean: i64 = row.get(4)?;
    let last_observed_at: String = row.get(6)?;
    Ok(StoredProjectState {
        project_id: ProjectId::from_string(row.get::<_, String>(0)?),
        repository_id: repository_id.map(RepositoryId::from_string),
        branch: row.get(2)?,
        head_commit: row.get(3)?,
        working_tree_clean: clean != 0,
        observation_version: row.get::<_, i64>(5)? as u64,
        last_observed_at: json::parse_timestamp(&last_observed_at)
            .map_err(|err| sqlite_conv_failure(6, err))?,
        state_version: row.get::<_, i64>(7)? as u64,
    })
}

/// The store-facing implementation of [`ProviderSyncRepository`].
#[derive(Clone)]
pub struct SqliteProviderSyncRepository {
    pool: crate::connection::ConnectionPool,
}

impl SqliteProviderSyncRepository {
    pub(crate) fn new(pool: crate::connection::ConnectionPool) -> Self {
        SqliteProviderSyncRepository { pool }
    }

    fn conn(&self) -> PooledConn {
        self.pool.get()
    }
}

#[async_trait]
impl director_domain::Store for SqliteProviderSyncRepository {
    type Error = StoreError;
}

#[async_trait]
impl director_domain::ProviderSyncRepository for SqliteProviderSyncRepository {
    async fn record_sync(&self, sync: &ProviderSync) -> Result<ProviderSync, StoreError> {
        let sync = sync.clone();
        let conn = self.conn();
        conn.execute(
            "INSERT INTO provider_sync (entity_id, provider, external_id, last_sync_at,
                                        last_success_at, last_error, external_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(entity_id, provider) DO UPDATE SET
                 external_id = excluded.external_id,
                 last_sync_at = excluded.last_sync_at,
                 last_success_at = excluded.last_success_at,
                 last_error = excluded.last_error,
                 external_version = excluded.external_version",
            rusqlite::params![
                sync.entity_id,
                sync.provider,
                sync.external_id,
                json::timestamp(sync.last_sync_at),
                sync.last_success_at.map(json::timestamp),
                sync.last_error,
                sync.external_version,
            ],
        )
        .map_err(translate_error)?;
        Ok(sync)
    }

    async fn last_sync(
        &self,
        entity_id: &str,
        provider: &str,
    ) -> Result<Option<ProviderSync>, StoreError> {
        let conn = self.conn();
        conn.query_row(
            "SELECT entity_id, provider, external_id, last_sync_at, last_success_at,
                    last_error, external_version
             FROM provider_sync WHERE entity_id = ?1 AND provider = ?2",
            rusqlite::params![entity_id, provider],
            row_to_provider_sync,
        )
        .optional()
        .map_err(translate_error)
    }
}

fn row_to_provider_sync(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProviderSync> {
    let last_sync_at: String = row.get(3)?;
    let last_success_at: Option<String> = row.get(4)?;
    Ok(ProviderSync {
        entity_id: row.get(0)?,
        provider: row.get(1)?,
        external_id: row.get(2)?,
        last_sync_at: json::parse_timestamp(&last_sync_at)
            .map_err(|err| sqlite_conv_failure(3, err))?,
        last_success_at: last_success_at
            .map(|text| json::parse_timestamp(&text))
            .transpose()
            .map_err(|err| sqlite_conv_failure(4, err))?,
        last_error: row.get(5)?,
        external_version: row.get(6)?,
    })
}

fn sqlite_conv_failure(index: usize, err: StoreError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, err.into())
}
