//! [`PlanRepository`] over SQLite.
//!
//! The behavior that matters here is the activation rule. A plan is created as
//! a draft and becomes authoritative only through [`activate_plan`], which
//! supersedes the project's currently active plan in the *same transaction* as
//! the promotion. A project is therefore never left with two active plans —
//! the partial unique index `idx_plans_active_per_project` is the second line
//! of defense, turning a forgotten supersession into an error rather than a
//! silent second authoritative plan.
//!
//! The superseded plan is marked, never deleted. "What did we used to believe,
//! and when" is the point of the entity, and that question is only answerable
//! if the old plan is still in the table.

use async_trait::async_trait;
use rusqlite::OptionalExtension;

use director_domain::ids::{AgentId, PlanId, ProjectId};
use director_domain::plan::{Plan, PlanStatus};
use director_domain::StoreError;

use crate::connection::{translate_error, PooledConn};
use crate::json;

/// The store-facing implementation of [`PlanRepository`].
#[derive(Clone)]
pub struct SqlitePlanRepository {
    pool: crate::connection::ConnectionPool,
}

impl SqlitePlanRepository {
    pub(crate) fn new(pool: crate::connection::ConnectionPool) -> Self {
        SqlitePlanRepository { pool }
    }

    fn conn(&self) -> PooledConn {
        self.pool.get()
    }
}

#[async_trait]
impl director_domain::Store for SqlitePlanRepository {
    type Error = StoreError;
}

#[async_trait]
impl director_domain::PlanRepository for SqlitePlanRepository {
    async fn create_plan(&self, plan: &Plan) -> Result<Plan, StoreError> {
        let plan = plan.clone();
        let conn = self.conn();
        conn.execute(
            "INSERT INTO plans (id, project_id, objective, task_ids, rationale, status,
                                supersedes, superseded_by, created_by, state_version,
                                created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                plan.id.as_str(),
                plan.project_id.as_str(),
                plan.objective,
                json::to_json(&plan.task_ids)?,
                plan.rationale,
                json::to_json(&plan.status)?,
                plan.supersedes.as_ref().map(PlanId::as_str),
                plan.superseded_by.as_ref().map(PlanId::as_str),
                plan.created_by.as_ref().map(AgentId::as_str),
                plan.state_version as i64,
                json::timestamp(plan.created_at),
                json::timestamp(plan.updated_at),
            ],
        )
        .map_err(translate_error)?;

        load_plan(&conn, &plan.id)
    }

    async fn get_plan(&self, id: &PlanId) -> Result<Plan, StoreError> {
        let conn = self.conn();
        load_plan(&conn, id)
    }

    async fn update_plan(&self, plan: &Plan) -> Result<Plan, StoreError> {
        let plan = plan.clone();
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(translate_error)?;

        let rows = tx
            .execute(
                "UPDATE plans
                SET objective = ?2, task_ids = ?3, rationale = ?4, status = ?5,
                    supersedes = ?6, superseded_by = ?7, created_by = ?8,
                    state_version = state_version + 1, updated_at = ?9
              WHERE id = ?1 AND state_version = ?10",
                rusqlite::params![
                    plan.id.as_str(),
                    plan.objective,
                    json::to_json(&plan.task_ids)?,
                    plan.rationale,
                    json::to_json(&plan.status)?,
                    plan.supersedes.as_ref().map(PlanId::as_str),
                    plan.superseded_by.as_ref().map(PlanId::as_str),
                    plan.created_by.as_ref().map(AgentId::as_str),
                    json::timestamp(chrono::Utc::now()),
                    plan.state_version as i64,
                ],
            )
            .map_err(translate_error)?;
        crate::connection::row_count_to_outcome(
            rows,
            "plan",
            plan.id.as_str(),
            plan.state_version,
        )?;

        tx.commit().map_err(translate_error)?;
        load_plan(&conn, &plan.id)
    }

    async fn activate_plan(&self, id: &PlanId, authorized_by: AgentId) -> Result<Plan, StoreError> {
        let id = id.clone();
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(translate_error)?;

        // The plan being promoted, as it currently stands. It must exist and
        // must still be a draft: activating an already-active plan is a no-op
        // someone got wrong, and activating a superseded plan would resurrect
        // history.
        let mut plan: Plan = tx
            .query_row(
                &format!("SELECT {PLAN_COLUMNS} FROM plans WHERE id = ?1"),
                [id.as_str()],
                row_to_plan,
            )
            .map_err(translate_error)?;

        if plan.status != PlanStatus::Draft {
            return Err(StoreError::ConstraintViolation(format!(
                "plan {} is {:?} and cannot be activated",
                plan.id, plan.status
            )));
        }

        // Supersede the sitting active plan, if there is one, and link both
        // ways. Same transaction as the promotion, so the two never diverge.
        // Status is compared as JSON, because that is how it is written: a bare
        // 'active' in the predicate never matches a column holding "active".
        let previous: Option<Plan> = tx
            .query_row(
                &format!("SELECT {PLAN_COLUMNS} FROM plans WHERE project_id = ?1 AND status = ?2"),
                rusqlite::params![
                    plan.project_id.as_str(),
                    json::to_json(&PlanStatus::Active)?
                ],
                row_to_plan,
            )
            .optional()
            .map_err(translate_error)?;

        if let Some(previous) = previous.as_ref() {
            tx.execute(
                "UPDATE plans SET status = ?2, superseded_by = ?3,
                                  state_version = state_version + 1, updated_at = ?4
                  WHERE id = ?1",
                rusqlite::params![
                    previous.id.as_str(),
                    json::to_json(&PlanStatus::Superseded)?,
                    id.as_str(),
                    json::timestamp(chrono::Utc::now()),
                ],
            )
            .map_err(translate_error)?;
            plan.supersedes = Some(previous.id.clone());
        }

        // Promote. `activate` records who authorized it; the state version the
        // caller read is the precondition, as on every other update.
        plan.activate(authorized_by);
        tx.execute(
            "UPDATE plans
                SET status = ?2, created_by = ?3, supersedes = ?4,
                    state_version = state_version + 1, updated_at = ?5
              WHERE id = ?1 AND state_version = ?6",
            rusqlite::params![
                plan.id.as_str(),
                json::to_json(&PlanStatus::Active)?,
                plan.created_by.as_ref().map(AgentId::as_str),
                plan.supersedes.as_ref().map(PlanId::as_str),
                json::timestamp(plan.updated_at),
                plan.state_version as i64,
            ],
        )
        .map_err(translate_error)?;

        tx.commit().map_err(translate_error)?;
        load_plan(&conn, &id)
    }

    async fn active_plan_for_project(
        &self,
        project: &ProjectId,
    ) -> Result<Option<Plan>, StoreError> {
        let conn = self.conn();
        conn.query_row(
            "SELECT id, project_id, objective, task_ids, rationale, status, supersedes,
                    superseded_by, created_by, state_version, created_at, updated_at
             FROM plans WHERE project_id = ?1 AND status = ?2",
            rusqlite::params![project.as_str(), json::to_json(&PlanStatus::Active)?],
            row_to_plan,
        )
        .optional()
        .map_err(translate_error)
    }

    async fn plans_for_project(&self, project: &ProjectId) -> Result<Vec<Plan>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, project_id, objective, task_ids, rationale, status, supersedes,
                        superseded_by, created_by, state_version, created_at, updated_at
                 FROM plans WHERE project_id = ?1 ORDER BY created_at DESC",
            )
            .map_err(translate_error)?;
        let rows = stmt
            .query_map([project.as_str()], row_to_plan)
            .map_err(translate_error)?;
        let mut all = Vec::new();
        for row in rows {
            all.push(row.map_err(translate_error)?);
        }
        Ok(all)
    }
}

/// The columns every plan query selects, in order. One place, so the SELECTs
/// and the row mapper cannot drift apart.
const PLAN_COLUMNS: &str = "id, project_id, objective, task_ids, rationale, status, supersedes,
                           superseded_by, created_by, state_version, created_at, updated_at";

/// Load one plan by id, or [`StoreError::NotFound`].
pub(crate) fn load_plan(conn: &PooledConn, id: &PlanId) -> Result<Plan, StoreError> {
    conn.query_row(
        &format!("SELECT {PLAN_COLUMNS} FROM plans WHERE id = ?1"),
        [id.as_str()],
        row_to_plan,
    )
    .map_err(translate_error)
}

/// Map a row into a [`Plan`]. A bad column is an error, not a default.
fn row_to_plan(row: &rusqlite::Row<'_>) -> rusqlite::Result<Plan> {
    let task_ids_text: String = row.get(3)?;
    let status_text: String = row.get(5)?;
    let supersedes: Option<String> = row.get(6)?;
    let superseded_by: Option<String> = row.get(7)?;
    let created_by: Option<String> = row.get(8)?;

    // Id newtypes are transparent over String, so the task ids round-trip as
    // plain JSON strings and read back as TaskId without a second pass.
    let task_ids: Vec<director_domain::ids::TaskId> =
        json::from_json(&task_ids_text).map_err(conv_failure(3))?;

    Ok(Plan {
        id: PlanId::from_string(row.get::<_, String>(0)?),
        project_id: ProjectId::from_string(row.get::<_, String>(1)?),
        objective: row.get(2)?,
        task_ids,
        rationale: row.get(4)?,
        status: json::from_json(&status_text).map_err(conv_failure(5))?,
        supersedes: supersedes.map(PlanId::from_string),
        superseded_by: superseded_by.map(PlanId::from_string),
        created_by: created_by.map(AgentId::from_string),
        state_version: row.get::<_, i64>(9)? as u64,
        created_at: json::parse_timestamp(&row.get::<_, String>(10)?).map_err(conv_failure(10))?,
        updated_at: json::parse_timestamp(&row.get::<_, String>(11)?).map_err(conv_failure(11))?,
    })
}

/// Lift a store error into the rusqlite shape a row mapper must return, pinning
/// the column index so the message says which field is at fault.
fn conv_failure(index: usize) -> impl Fn(director_domain::StoreError) -> rusqlite::Error + 'static {
    move |err: director_domain::StoreError| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, err.into())
    }
}
