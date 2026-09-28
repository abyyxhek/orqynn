//! [`AgentRepository`] over SQLite.
//!
//! An agent row is a registry entry — who is available, on which machine, in
//! which harness. What the agent is *working on* is not here; it is in
//! [`crate::assignments`]. The `current_task` column is a denormalized view the
//! store keeps for fast lookups, and the assignment record remains the
//! authority, exactly as the domain documents.

use async_trait::async_trait;

use director_domain::agent::{Agent, Harness};
use director_domain::ids::{AgentId, MachineId, TaskId};
use director_domain::StoreError;

use crate::connection::{row_count_to_outcome, translate_error, PooledConn};
use crate::json;

/// The store-facing implementation of [`AgentRepository`].
#[derive(Clone)]
pub struct SqliteAgentRepository {
    pool: crate::connection::ConnectionPool,
}

impl SqliteAgentRepository {
    pub(crate) fn new(pool: crate::connection::ConnectionPool) -> Self {
        SqliteAgentRepository { pool }
    }

    fn conn(&self) -> PooledConn {
        self.pool.get()
    }
}

#[async_trait]
impl director_domain::Store for SqliteAgentRepository {
    type Error = StoreError;
}

#[async_trait]
impl director_domain::AgentRepository for SqliteAgentRepository {
    async fn register_agent(&self, agent: &Agent) -> Result<Agent, StoreError> {
        let agent = agent.clone();
        let conn = self.conn();
        conn.execute(
            "INSERT INTO agents (id, name, harness, model, machine, capabilities, status,
                                 current_task, provider, metadata, state_version,
                                 registered_at, last_seen, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            rusqlite::params![
                agent.id.as_str(),
                agent.name,
                json::to_json(&agent.harness)?,
                agent.model,
                agent.machine.as_str(),
                json::to_json(&agent.capabilities)?,
                json::to_json(&agent.status)?,
                agent.current_task.as_ref().map(TaskId::as_str),
                agent.provider,
                json::to_json_or_null(Some(&agent.metadata))?,
                agent.state_version as i64,
                json::timestamp(agent.registered_at),
                json::timestamp(agent.last_seen),
                agent.updated_at.map(json::timestamp),
            ],
        )
        .map_err(translate_error)?;
        load_agent(&conn, &agent.id)
    }

    async fn get_agent(&self, id: &AgentId) -> Result<Agent, StoreError> {
        let conn = self.conn();
        load_agent(&conn, id)
    }

    async fn update_agent(&self, agent: &Agent) -> Result<Agent, StoreError> {
        let agent = agent.clone();
        let conn = self.conn();
        let rows = conn
            .execute(
                "UPDATE agents
                    SET name = ?2, harness = ?3, model = ?4, machine = ?5,
                        capabilities = ?6, status = ?7, current_task = ?8, provider = ?9,
                        metadata = ?10, last_seen = ?11, updated_at = ?12,
                        state_version = state_version + 1
                  WHERE id = ?1 AND state_version = ?13",
                rusqlite::params![
                    agent.id.as_str(),
                    agent.name,
                    json::to_json(&agent.harness)?,
                    agent.model,
                    agent.machine.as_str(),
                    json::to_json(&agent.capabilities)?,
                    json::to_json(&agent.status)?,
                    agent.current_task.as_ref().map(TaskId::as_str),
                    agent.provider,
                    json::to_json_or_null(Some(&agent.metadata))?,
                    json::timestamp(agent.last_seen),
                    json::timestamp(chrono::Utc::now()),
                    agent.state_version as i64,
                ],
            )
            .map_err(translate_error)?;
        row_count_to_outcome(rows, "agent", agent.id.as_str(), agent.state_version)?;
        load_agent(&conn, &agent.id)
    }

    async fn list_agents(&self) -> Result<Vec<Agent>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, harness, model, machine, capabilities, status, current_task,
                        provider, metadata, state_version, registered_at, last_seen, updated_at
                 FROM agents ORDER BY name",
            )
            .map_err(translate_error)?;
        let rows = stmt.query_map([], row_to_agent).map_err(translate_error)?;
        let mut all = Vec::new();
        for row in rows {
            all.push(row.map_err(translate_error)?);
        }
        Ok(all)
    }
}

/// Load one agent by id, or [`StoreError::NotFound`].
pub(crate) fn load_agent(conn: &PooledConn, id: &AgentId) -> Result<Agent, StoreError> {
    conn.query_row(
        "SELECT id, name, harness, model, machine, capabilities, status, current_task,
                provider, metadata, state_version, registered_at, last_seen, updated_at
         FROM agents WHERE id = ?1",
        [id.as_str()],
        row_to_agent,
    )
    .map_err(translate_error)
}

/// Update an agent's denormalized `current_task` view without going through the
/// optimistic path — this is a reconciliation write performed by the assignment
/// service alongside the assignment record it reflects, so it takes the
/// version it expects to be writing against.
pub(crate) fn set_current_task(
    conn: &PooledConn,
    id: &AgentId,
    task: Option<&TaskId>,
    caller_version: u64,
) -> Result<(), StoreError> {
    let rows = conn
        .execute(
            "UPDATE agents SET current_task = ?2, updated_at = ?3, state_version = state_version + 1
              WHERE id = ?1 AND state_version = ?4",
            rusqlite::params![
                id.as_str(),
                task.map(TaskId::as_str),
                json::timestamp(chrono::Utc::now()),
                caller_version as i64,
            ],
        )
        .map_err(translate_error)?;
    row_count_to_outcome(rows, "agent", id.as_str(), caller_version)
}

fn row_to_agent(row: &rusqlite::Row<'_>) -> rusqlite::Result<Agent> {
    let model: Option<String> = row.get(3)?;
    let current_task: Option<String> = row.get(7)?;
    let provider: Option<String> = row.get(8)?;
    let metadata: Option<String> = row.get(9)?;
    let registered_at: String = row.get(11)?;
    let last_seen: String = row.get(12)?;
    let updated_at: Option<String> = row.get(13)?;

    let mut agent = Agent::register(
        AgentId::from_string(row.get::<_, String>(0)?),
        String::new(),
        Harness::ClaudeCode,
        MachineId::from_string("MACH-unknown"),
        vec![],
    );
    agent.name = row.get(1)?;
    agent.harness =
        json::from_json(&row.get::<_, String>(2)?).map_err(|err| sqlite_conv_failure(2, err))?;
    agent.model = model;
    agent.machine = MachineId::from_string(row.get::<_, String>(4)?);
    agent.capabilities =
        json::from_json(&row.get::<_, String>(5)?).map_err(|err| sqlite_conv_failure(5, err))?;
    agent.status =
        json::from_json(&row.get::<_, String>(6)?).map_err(|err| sqlite_conv_failure(6, err))?;
    agent.current_task = current_task.map(TaskId::from_string);
    agent.provider = provider;
    agent.metadata = json::from_json_or_none(metadata.as_deref())
        .map_err(|err| sqlite_conv_failure(9, err))?
        .unwrap_or(serde_json::Value::Null);
    agent.state_version = row.get::<_, i64>(10)? as u64;
    agent.registered_at =
        json::parse_timestamp(&registered_at).map_err(|err| sqlite_conv_failure(11, err))?;
    agent.last_seen =
        json::parse_timestamp(&last_seen).map_err(|err| sqlite_conv_failure(12, err))?;
    agent.updated_at = updated_at
        .map(|text| json::parse_timestamp(&text))
        .transpose()
        .map_err(|err| sqlite_conv_failure(13, err))?;
    Ok(agent)
}

fn sqlite_conv_failure(index: usize, err: StoreError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, err.into())
}
