//! [`DecisionRepository`] over SQLite.
//!
//! The behavior that matters here is that a reversed decision is *marked*, not
//! deleted. [`supersede_decision`] keeps the row and adds a `superseded_by`
//! link, so the record stays honest when the project changes its mind: "we used
//! to believe X, and here is the decision that reversed it" is an ordinary
//! query rather than a data-loss event.

use async_trait::async_trait;

use director_domain::decision::{Decision, DecisionStatus};
use director_domain::ids::{AgentId, DecisionId, TaskId};
use director_domain::StoreError;

use crate::connection::{translate_error, PooledConn};
use crate::json;

/// The store-facing implementation of [`DecisionRepository`].
#[derive(Clone)]
pub struct SqliteDecisionRepository {
    pool: crate::connection::ConnectionPool,
}

impl SqliteDecisionRepository {
    pub(crate) fn new(pool: crate::connection::ConnectionPool) -> Self {
        SqliteDecisionRepository { pool }
    }

    fn conn(&self) -> PooledConn {
        self.pool.get()
    }
}

#[async_trait]
impl director_domain::Store for SqliteDecisionRepository {
    type Error = StoreError;
}

#[async_trait]
impl director_domain::DecisionRepository for SqliteDecisionRepository {
    async fn create_decision(&self, decision: &Decision) -> Result<Decision, StoreError> {
        let decision = decision.clone();
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(translate_error)?;
        insert_decision(&tx, &decision)?;
        tx.commit().map_err(translate_error)?;

        load_decision(&conn, &decision.id)
    }

    async fn get_decision(&self, id: &DecisionId) -> Result<Decision, StoreError> {
        let conn = self.conn();
        load_decision(&conn, id)
    }

    async fn supersede_decision(
        &self,
        id: &DecisionId,
        superseded_by: &DecisionId,
    ) -> Result<Decision, StoreError> {
        let id = id.clone();
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(translate_error)?;

        // The decision being reversed, as it stands. It must still stand —
        // reversing an already-reversed decision would lose the first reversal
        // and leave two decisions pointing at the same successor.
        let mut decision: Decision = tx
            .query_row(
                &format!("SELECT {DECISION_COLUMNS} FROM decisions WHERE id = ?1"),
                [id.as_str()],
                row_to_decision,
            )
            .map_err(translate_error)?;

        if decision.status != DecisionStatus::Active {
            return Err(StoreError::ConstraintViolation(format!(
                "decision {} is {:?} and cannot be superseded",
                decision.id, decision.status
            )));
        }

        decision.supersede(superseded_by.clone());
        tx.execute(
            "UPDATE decisions SET status = ?2, superseded_by = ?3,
                                  state_version = state_version + 1, updated_at = ?4
              WHERE id = ?1",
            rusqlite::params![
                decision.id.as_str(),
                json::to_json(&decision.status)?,
                decision.superseded_by.as_ref().map(DecisionId::as_str),
                json::timestamp(decision.updated_at),
            ],
        )
        .map_err(translate_error)?;

        tx.commit().map_err(translate_error)?;
        load_decision(&conn, &id)
    }

    async fn decisions_for_task(&self, task: &TaskId) -> Result<Vec<Decision>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, task_id, title, rationale, alternatives, status, superseded_by,
                        made_by, state_version, made_at, updated_at
                 FROM decisions WHERE task_id = ?1 ORDER BY made_at DESC",
            )
            .map_err(translate_error)?;
        let rows = stmt
            .query_map([task.as_str()], row_to_decision)
            .map_err(translate_error)?;
        let mut all = Vec::new();
        for row in rows {
            all.push(row.map_err(translate_error)?);
        }
        Ok(all)
    }
}

/// The columns every decision query selects, in order. One place, so the
/// SELECTs and the row mapper cannot drift apart.
const DECISION_COLUMNS: &str = "id, task_id, title, rationale, alternatives, status,
                               superseded_by, made_by, state_version, made_at, updated_at";

/// Load one decision by id, or [`StoreError::NotFound`].
pub(crate) fn load_decision(conn: &PooledConn, id: &DecisionId) -> Result<Decision, StoreError> {
    conn.query_row(
        &format!("SELECT {DECISION_COLUMNS} FROM decisions WHERE id = ?1"),
        [id.as_str()],
        row_to_decision,
    )
    .map_err(translate_error)
}

/// Insert a decision row inside the caller's transaction.
///
/// Called from [`DecisionRepository::create_decision`] and from
/// [`crate::tasks::cancel_task`], which is the reason it exists: a cancellation
/// and the decision that records why it happened have to land in one
/// transaction, so the decision write is a callable half rather than a method
/// the composite operation has to reconstruct.
pub(crate) fn insert_decision(
    tx: &rusqlite::Transaction<'_>,
    decision: &Decision,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO decisions (id, task_id, title, rationale, alternatives, status,
                                superseded_by, made_by, state_version, made_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            decision.id.as_str(),
            decision.task_id.as_ref().map(TaskId::as_str),
            decision.title,
            decision.rationale,
            json::to_json(&decision.alternatives_considered)?,
            json::to_json(&decision.status)?,
            decision.superseded_by.as_ref().map(DecisionId::as_str),
            decision.made_by.as_ref().map(AgentId::as_str),
            decision.state_version as i64,
            json::timestamp(decision.made_at),
            json::timestamp(decision.updated_at),
        ],
    )
    .map_err(translate_error)?;
    Ok(())
}

/// Map a row into a [`Decision`]. A bad column is an error, not a default.
fn row_to_decision(row: &rusqlite::Row<'_>) -> rusqlite::Result<Decision> {
    let task_id: Option<String> = row.get(1)?;
    let alternatives_text: String = row.get(4)?;
    let status_text: String = row.get(5)?;
    let superseded_by: Option<String> = row.get(6)?;
    let made_by: Option<String> = row.get(7)?;

    let mut decision = Decision::new(
        DecisionId::from_string(row.get::<_, String>(0)?),
        String::new(),
        String::new(),
    );
    decision.task_id = task_id.map(TaskId::from_string);
    decision.title = row.get(2)?;
    decision.rationale = row.get(3)?;
    decision.alternatives_considered =
        json::from_json(&alternatives_text).map_err(conv_failure(4))?;
    decision.status = json::from_json(&status_text).map_err(conv_failure(5))?;
    decision.superseded_by = superseded_by.map(DecisionId::from_string);
    decision.made_by = made_by.map(AgentId::from_string);
    decision.state_version = row.get::<_, i64>(8)? as u64;
    decision.made_at = json::parse_timestamp(&row.get::<_, String>(9)?).map_err(conv_failure(9))?;
    decision.updated_at =
        json::parse_timestamp(&row.get::<_, String>(10)?).map_err(conv_failure(10))?;
    Ok(decision)
}

/// Lift a store error into the rusqlite shape a row mapper must return, pinning
/// the column index so the message says which field is at fault.
fn conv_failure(index: usize) -> impl Fn(director_domain::StoreError) -> rusqlite::Error + 'static {
    move |err: director_domain::StoreError| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, err.into())
    }
}
