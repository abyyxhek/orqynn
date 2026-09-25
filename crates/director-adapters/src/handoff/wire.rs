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
    /// Caller-visible task id, e.g. `"AUTH-42"` or a server-generated `"t3"`.
    pub id: String,
    /// Human-readable title, set at creation.
    pub title: String,
    /// One of the substrate's six statuses, as a raw string — mapped, not
    /// trusted, in [`crate::handoff::mapping`].
    pub status: String,
    /// Free-form notes attached to the task. Absent until someone writes them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// `low` / `medium` / `high`. Director's `Critical` has no wire equivalent
    /// and rides in [`TaskData::extra`] instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// Server-assigned creation timestamp, as an unparsed string.
    pub created_at: String,
    /// Server-assigned last-modification timestamp, as an unparsed string.
    pub updated_at: String,
    /// Set by the substrate only when the task reaches a terminal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    /// Free-form labels; empty until applied.
    #[serde(default)]
    pub labels: Vec<String>,
    /// URLs attached to the task with no typed relationship.
    #[serde(default)]
    pub links: Vec<String>,
    /// Typed links to other artifacts (commits, PRs, docs).
    #[serde(default)]
    pub task_links: Vec<TaskLink>,
    /// Acceptance criteria — self-reportable checkboxes on the substrate side.
    #[serde(default)]
    pub done_criteria: Vec<DoneCriterion>,
    /// Scheduling block. Read back; Director never writes the estimate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<Schedule>,
    /// Ids this task waits on before it becomes ready.
    #[serde(default)]
    pub dependencies: Vec<String>,
    /// Present on read when other tasks depend on this one; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependents: Option<Vec<String>>,
    /// Manual ordering hint within a list, when the substrate records one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<u32>,
    /// The agent currently holding the task, when assigned. Assignment is
    /// authoritative in Director's own layer, not here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// Cross-process claim lease. Present only while the task is claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<TaskLock>,
    /// Scope paths the task is allowed to touch, for overlap detection.
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
    /// Link kind — e.g. `commit`, `pr`, `doc` — renamed from the wire's `type`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Optional human label for the link.
    #[serde(default)]
    pub label: Option<String>,
    /// The link target as a URL or reference string.
    pub url: String,
}

/// One acceptance criterion. On the substrate side this is a self-reportable
/// checkbox — the reason Director's verification engine (Phase 10) never trusts
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoneCriterion {
    /// The criterion's wording.
    pub text: String,
    /// Whether some agent reported the criterion met. **Never** treated as
    /// verification by Director — see Phase 0 finding R6.
    #[serde(default)]
    pub checked: bool,
}

/// Scheduling/estimate block. Director deliberately never writes
/// `estimate_hours` — see [`crate::handoff`] docs — but must read it back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    /// Estimated effort. Blocking when `require_estimate_hours` is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate_hours: Option<f64>,
    /// Reported actual effort, when recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_hours: Option<f64>,
    /// Planned start, as an unparsed string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_date: Option<String>,
    /// Planned due date, as an unparsed string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_date: Option<String>,
}

/// A claim lease on a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskLock {
    /// The agent holding the claim.
    pub agent_id: String,
    /// The session that made the claim.
    pub session_id: String,
    /// When the claim was taken, as an unparsed string.
    pub claimed_at: String,
    /// When the lease expires, as an unparsed string.
    pub lease_expires_at: String,
    /// Lease duration in seconds.
    pub lease_ttl_seconds: u64,
}

/// One agent record, as `handoff_list_agents` returns them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRecord {
    /// The agent's identifier.
    pub agent_id: String,
    /// The agent's current session, when one is live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Working tree path the agent operates in.
    pub worktree: String,
    /// Branch the agent is on, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Active | Stale | Disconnected, recomputed from heartbeat age by the
    /// server on every call.
    pub status: String,
    /// When the agent registered, as an unparsed string.
    pub registered_at: String,
    /// Its most recent heartbeat, as an unparsed string.
    pub last_heartbeat: String,
    /// Only present when the caller asks for `include_tasks`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claimed_tasks: Vec<String>,
}

/// Wrapper shape of `handoff_list_agents`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentList {
    /// All agent records in this project.
    pub agents: Vec<AgentRecord>,
    /// Total count the server reports alongside the page.
    pub total: u32,
}

/// One session summary, as `handoff_list_sessions` returns them.
///
/// Note this is a *summary*, not the full `SessionData`: `handoff_list_sessions`
/// reports counts and progress rather than the full decisions/checklist arrays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// The session's id.
    pub id: String,
    /// Lifecycle status, as a raw string.
    pub status: String,
    /// Human-readable summary the session recorded on close.
    pub summary: String,
    /// When the session started, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// When the session ended, when it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// Branch the session worked on, when recorded.
    #[serde(default)]
    pub branch: Option<String>,
    /// Head commit the session reached, when recorded.
    #[serde(default)]
    pub commit: Option<String>,
    /// How many decisions the session logged.
    #[serde(default)]
    pub decisions_count: u32,
    /// Progress summary like `"3/7"`, as a raw string.
    #[serde(default)]
    pub checklist_progress: String,
}

/// Wrapper shape of `handoff_list_tasks` — a tree of summaries, not full tasks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTree {
    /// The task summaries, in tree order.
    pub task_tree: Vec<TaskTreeNode>,
}

/// One node of a task-tree summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTreeNode {
    /// The task's id.
    pub id: String,
    /// The task's title.
    pub title: String,
    /// Its status, as a raw string.
    pub status: String,
    /// Nested children, empty at a leaf.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TaskTreeNode>,
}
