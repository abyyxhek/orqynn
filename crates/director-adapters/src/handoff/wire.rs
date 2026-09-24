//! Serde mirrors of handoff-mcp's JSON shapes.
//!
//! These are deliberately **not** the domain types and can never become them.
//! They describe what the substrate puts on the wire, warts and all, so that
//! every schema difference between it and Director's model is visible in one
//! place — [`crate::handoff::mapping`] — instead of scattered through call
//! sites.
//!
//! Verified against the live v0.35.1 server by round-tripping real requests.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// handoff-mcp's task record, as returned by `handoff_get_task`.
///
/// Field names are exactly what the server sends. Option-ality matches what a
/// fresh task omits: `notes`, `completed_at`, `schedule`, `order`, `assignee`,
/// and `lock` are absent until set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskData {
    pub id: String,
    pub title: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub links: Vec<String>,
    #[serde(default)]
    pub task_links: Vec<TaskLink>,
    #[serde(default)]
    pub done_criteria: Vec<DoneCriterion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<Schedule>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    /// Present on read when other tasks depend on this one; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependents: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// Cross-process claim lease. Present only while the task is claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<TaskLock>,
    #[serde(default)]
    pub scope_paths: Vec<String>,
    /// `#[serde(flatten)]` on the server side, so Director-only state rides
    /// here. This is how non-isomorphic statuses survive a round trip.
    #[serde(default, flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// A typed link attached to a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskLink {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub label: Option<String>,
    pub url: String,
}

/// One acceptance criterion. On the substrate side this is a self-reportable
/// checkbox — the reason Director's verification engine (Phase 10) never trusts
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoneCriterion {
    pub text: String,
    #[serde(default)]
    pub checked: bool,
}

/// Scheduling/estimate block. Director deliberately never writes
/// `estimate_hours` — see [`crate::handoff`] docs — but must read it back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate_hours: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_hours: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_date: Option<String>,
}

/// A claim lease on a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskLock {
    pub agent_id: String,
    pub session_id: String,
    pub claimed_at: String,
    pub lease_expires_at: String,
    pub lease_ttl_seconds: u64,
}

/// One agent record, as `handoff_list_agents` returns them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub worktree: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Active | Stale | Disconnected, recomputed from heartbeat age by the
    /// server on every call.
    pub status: String,
    pub registered_at: String,
    pub last_heartbeat: String,
    /// Only present when the caller asks for `include_tasks`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claimed_tasks: Vec<String>,
}

/// Wrapper shape of `handoff_list_agents`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentList {
    pub agents: Vec<AgentRecord>,
    pub total: u32,
}

/// One session summary, as `handoff_list_sessions` returns them.
///
/// Note this is a *summary*, not the full `SessionData`: `handoff_list_sessions`
/// reports counts and progress rather than the full decisions/checklist arrays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub status: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub decisions_count: u32,
    #[serde(default)]
    pub checklist_progress: String,
}

/// Wrapper shape of `handoff_list_tasks` — a tree of summaries, not full tasks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTree {
    pub task_tree: Vec<TaskTreeNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTreeNode {
    pub id: String,
    pub title: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TaskTreeNode>,
}
