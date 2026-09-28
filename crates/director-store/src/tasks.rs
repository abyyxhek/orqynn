//! [`TaskRepository`] over SQLite.
//!
//! The task row is the straightforward part. Two behaviors here are the reason
//! this file is not just CRUD:
//!
//! - **Optimistic concurrency.** [`update_task`] writes with
//!   `WHERE state_version = ?`, so a stale caller matches no row and gets a
//!   [`StoreError::StateVersionConflict`] instead of overwriting a newer
//!   version.
//! - **Append-only status history.** A status change is written to
//!   `task_status_history` as a new row, never as an update to an existing one.
//!   That is what makes "what was the task state before?" answerable after the
//!   fact, and it is done inside the same transaction as the task update — so
//!   a task never moves without its history recording the move.
//!
//! And one deliberate absence: there is no `current_agent_id` anywhere in this
//! module. Assignment lives in [`crate::assignments`], and the task row never
//! names an agent.

use async_trait::async_trait;
use rusqlite::OptionalExtension;

use director_domain::ids::{ProjectId, TaskId};
use director_domain::task::{Task, TaskStatus};
use director_domain::{StoreError, TaskStatusTransition};

use crate::connection::{row_count_to_outcome, translate_error, PooledConn};
use crate::json;

/// The store-facing implementation of [`TaskRepository`].
#[derive(Clone)]
pub struct SqliteTaskRepository {
    pool: crate::connection::ConnectionPool,
}

impl SqliteTaskRepository {
    pub(crate) fn new(pool: crate::connection::ConnectionPool) -> Self {
        SqliteTaskRepository { pool }
    }

    fn conn(&self) -> PooledConn {
        self.pool.get()
    }
}

#[async_trait]
impl director_domain::Store for SqliteTaskRepository {
    type Error = StoreError;
}

#[async_trait]
impl director_domain::TaskRepository for SqliteTaskRepository {
    async fn create_task(&self, task: &Task) -> Result<Task, StoreError> {
        let task = task.clone();
        let mut conn = self.conn();

        // A task must belong to a project. The store rejects an unscoped task
        // rather than inventing a project to hang it on.
        let project_id = task.project_id.as_ref().ok_or_else(|| {
            StoreError::ConstraintViolation(format!(
                "task {} has no project; Director's store requires one",
                task.id
            ))
        })?;

        let tx = conn.transaction().map_err(translate_error)?;
        tx.execute(
            "INSERT INTO tasks (id, project_id, title, objective, description, status,
                                priority, complexity, expected_outputs, dependencies,
                                scope_paths, required_capabilities, subtasks,
                                state_version, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            rusqlite::params![
                task.id.as_str(),
                project_id.as_str(),
                task.title,
                task.objective,
                task.description,
                json::to_json(&task.status)?,
                json::to_json_or_null(task.priority.as_ref())?,
                json::to_json(&task.complexity)?,
                json::to_json(&task.expected_outputs)?,
                json::to_json(&task.dependencies)?,
                json::to_json(&task.scope_paths)?,
                json::to_json(&task.required_capabilities)?,
                json::to_json(&task.subtasks)?,
                task.state_version as i64,
                json::timestamp(task.created_at),
                json::timestamp(task.updated_at),
            ],
        )
        .map_err(translate_error)?;

        // The queryable dependency graph, kept alongside the JSON column.
        replace_dependencies(&tx, &task.id, &task.dependencies)?;

        tx.commit().map_err(translate_error)?;
        load_task(&conn, &task.id)
    }

    async fn get_task(&self, id: &TaskId) -> Result<Task, StoreError> {
        let conn = self.conn();
        load_task(&conn, id)
    }

    async fn update_task(&self, task: &Task) -> Result<Task, StoreError> {
        let task = task.clone();
        let mut conn = self.conn();

        // What the task's status is now, so this update can record a transition
        // if the caller is moving it — and record nothing if it is not, so the
        // history does not accumulate no-op entries.
        let previous_status: Option<TaskStatus> = conn
            .query_row(
                "SELECT status FROM tasks WHERE id = ?1",
                [task.id.as_str()],
                |row| {
                    let text: String = row.get(0)?;
                    Ok(text)
                },
            )
            .optional()
            .map_err(translate_error)?
            .map(|text| json::from_json::<TaskStatus>(&text))
            .transpose()?;

        let tx = conn.transaction().map_err(translate_error)?;
        let rows = tx
            .execute(
                "UPDATE tasks
                    SET title = ?2, objective = ?3, description = ?4, status = ?5,
                        priority = ?6, complexity = ?7, expected_outputs = ?8,
                        dependencies = ?9, scope_paths = ?10, required_capabilities = ?11,
                        subtasks = ?12, state_version = state_version + 1, updated_at = ?13
                  WHERE id = ?1 AND state_version = ?14",
                rusqlite::params![
                    task.id.as_str(),
                    task.title,
                    task.objective,
                    task.description,
                    json::to_json(&task.status)?,
                    json::to_json_or_null(task.priority.as_ref())?,
                    json::to_json(&task.complexity)?,
                    json::to_json(&task.expected_outputs)?,
                    json::to_json(&task.dependencies)?,
                    json::to_json(&task.scope_paths)?,
                    json::to_json(&task.required_capabilities)?,
                    json::to_json(&task.subtasks)?,
                    json::timestamp(chrono::Utc::now()),
                    task.state_version as i64,
                ],
            )
            .map_err(translate_error)?;
        row_count_to_outcome(rows, "task", task.id.as_str(), task.state_version)?;

        replace_dependencies(&tx, &task.id, &task.dependencies)?;

        // Record the transition if there was one. Same transaction as the
        // update: the history never diverges from the row it describes.
        if let Some(from) = previous_status {
            if from != task.status {
                tx.execute(
                    "INSERT INTO task_status_history
                        (task_id, from_status, to_status, occurred_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![
                        task.id.as_str(),
                        json::to_json(&from)?,
                        json::to_json(&task.status)?,
                        json::timestamp(chrono::Utc::now()),
                    ],
                )
                .map_err(translate_error)?;
            }
        }

        tx.commit().map_err(translate_error)?;
        load_task(&conn, &task.id)
    }

    async fn list_tasks(&self, project: &ProjectId) -> Result<Vec<Task>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, project_id, title, objective, description, status, priority,
                        complexity, expected_outputs, dependencies, scope_paths,
                        required_capabilities, subtasks, state_version, created_at,
                        updated_at
                 FROM tasks WHERE project_id = ?1 ORDER BY created_at",
            )
            .map_err(translate_error)?;
        let rows = stmt
            .query_map([project.as_str()], row_to_task)
            .map_err(translate_error)?;
        let mut all = Vec::new();
        for row in rows {
            all.push(row.map_err(translate_error)?);
        }
        Ok(all)
    }

    async fn task_history(&self, task: &TaskId) -> Result<Vec<TaskStatusTransition>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT task_id, from_status, to_status, occurred_at
                 FROM task_status_history WHERE task_id = ?1 ORDER BY id",
            )
            .map_err(translate_error)?;
        let rows = stmt
            .query_map([task.as_str()], |row| {
                let task_id: String = row.get(0)?;
                let from: String = row.get(1)?;
                let to: String = row.get(2)?;
                let at: String = row.get(3)?;
                let from = json::from_json::<TaskStatus>(&from)
                    .map_err(|err| sqlite_conv_failure(1, err))?;
                let to = json::from_json::<TaskStatus>(&to)
                    .map_err(|err| sqlite_conv_failure(2, err))?;
                let at = json::parse_timestamp(&at).map_err(|err| sqlite_conv_failure(3, err))?;
                Ok(TaskStatusTransition {
                    task_id: TaskId::from_string(task_id),
                    from,
                    to,
                    at,
                })
            })
            .map_err(translate_error)?;
        let mut all = Vec::new();
        for row in rows {
            all.push(row.map_err(translate_error)?);
        }
        Ok(all)
    }
}

/// Rewrite the dependency rows for a task from its current dependency list.
/// Called inside the task's transaction, so the graph and the JSON column are
/// always in step.
fn replace_dependencies(
    tx: &rusqlite::Transaction<'_>,
    task_id: &TaskId,
    dependencies: &[TaskId],
) -> Result<(), StoreError> {
    tx.execute(
        "DELETE FROM task_dependencies WHERE task_id = ?1",
        [task_id.as_str()],
    )
    .map_err(translate_error)?;
    for dep in dependencies {
        tx.execute(
            "INSERT INTO task_dependencies (task_id, dependency_id) VALUES (?1, ?2)
             ON CONFLICT DO NOTHING",
            rusqlite::params![task_id.as_str(), dep.as_str()],
        )
        .map_err(translate_error)?;
    }
    Ok(())
}

/// Load one task by id, or [`StoreError::NotFound`].
pub(crate) fn load_task(conn: &PooledConn, id: &TaskId) -> Result<Task, StoreError> {
    conn.query_row(
        "SELECT id, project_id, title, objective, description, status, priority,
                complexity, expected_outputs, dependencies, scope_paths,
                required_capabilities, subtasks, state_version, created_at, updated_at
         FROM tasks WHERE id = ?1",
        [id.as_str()],
        row_to_task,
    )
    .map_err(translate_error)
}

/// Map a row into a [`Task`]. Any malformed column is an error rather than a
/// silent default — see [`crate::json`] for why.
fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    let project_id: Option<String> = row.get(1)?;
    let description: Option<String> = row.get(4)?;
    let status: String = row.get(5)?;
    let priority: Option<String> = row.get(6)?;
    let complexity: String = row.get(7)?;
    let expected_outputs: String = row.get(8)?;
    let dependencies: String = row.get(9)?;
    let scope_paths: String = row.get(10)?;
    let required_capabilities: String = row.get(11)?;
    let subtasks: String = row.get(12)?;
    let created_at: String = row.get(14)?;
    let updated_at: String = row.get(15)?;

    let mut task = Task::new(
        TaskId::from_string(row.get::<_, String>(0)?),
        String::new(),
        String::new(),
    );
    task.project_id = project_id.map(ProjectId::from_string);
    task.title = row.get(2)?;
    task.objective = row.get(3)?;
    task.description = description;
    task.status = json::from_json(&status).map_err(|err| sqlite_conv_failure(5, err))?;
    task.priority =
        json::from_json_or_none(priority.as_deref()).map_err(|err| sqlite_conv_failure(6, err))?;
    task.complexity = json::from_json(&complexity).map_err(|err| sqlite_conv_failure(7, err))?;
    task.expected_outputs =
        json::from_json(&expected_outputs).map_err(|err| sqlite_conv_failure(8, err))?;
    task.dependencies =
        json::from_json(&dependencies).map_err(|err| sqlite_conv_failure(9, err))?;
    task.scope_paths = json::from_json(&scope_paths).map_err(|err| sqlite_conv_failure(10, err))?;
    task.required_capabilities =
        json::from_json(&required_capabilities).map_err(|err| sqlite_conv_failure(11, err))?;
    task.subtasks = json::from_json(&subtasks).map_err(|err| sqlite_conv_failure(12, err))?;
    task.state_version = row.get::<_, i64>(13)? as u64;
    task.created_at =
        json::parse_timestamp(&created_at).map_err(|err| sqlite_conv_failure(14, err))?;
    task.updated_at =
        json::parse_timestamp(&updated_at).map_err(|err| sqlite_conv_failure(15, err))?;
    Ok(task)
}

fn sqlite_conv_failure(index: usize, err: StoreError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, err.into())
}
