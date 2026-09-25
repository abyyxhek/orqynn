//! Bidirectional mapping between handoff-mcp's wire types and Director's domain.
//!
//! ## Why this file exists and why it is not trivial
//!
//! The two models are **not isomorphic**, and pretending otherwise would
//! silently corrupt state. Three vocabularies do not line up, and each is
//! handled explicitly:
//!
//! 1. **Task status.** The substrate has 6 states (`todo`, `in_progress`,
//!    `review`, `done`, `blocked`, `skipped`). Director has 8
//!    (`Backlog`, `Todo`, `InProgress`, `Blocked`, `VerificationPending`,
//!    `Done`, `Failed`, `Cancelled`). Director-only states ride in
//!    `TaskData.extra` and fall back to the nearest honest substrate state.
//!
//! 2. **The important one.** handoff-mcp's `done` is an agent self-report
//!    (Phase 0 finding R6: `handoff_check_criterion` is a checkbox an agent
//!    ticks). So a substrate `done` does **not** become Director `Done` — it
//!    becomes `VerificationPending`, because nothing has been verified.
//!    Only Director-initiated transitions, marked with an extra flag, map back
//!    to `Done`. This is the acceptance criterion "agent claims done, tests
//!    fail → must not become COMPLETED", enforced at the boundary.
//!
//! 3. **Priority.** The substrate has `low`/`medium`/`high`. Director adds
//!    `Critical`, which maps down to `high` and is recovered from `extra`.
//!
//! ## The `extra` channel, and what it can and cannot carry
//!
//! `TaskData.extra` is a real `#[serde(flatten)]` map *inside* the substrate's
//! storage layer, and this mapping writes Director-only state into it. But
//! against the live v0.35.1 server that channel is closed at the MCP boundary
//! in both directions:
//!
//! - **Reads.** `handoff_get_task` builds its reply from named fields only;
//!   `extra` is never serialized onto the wire.
//! - **Writes.** `handoff_update_task` reconstructs the record from named
//!   fields. A *create* starts from `extra: HashMap::new()`, and an *update*
//!   copies only known fields out of the request — so values Director sends
//!   in `extra` never reach the file.
//!
//! The consequence is stated plainly because it is easy to get wrong: **a
//! Director-only status or a `Critical` priority does not survive a substrate
//! round trip.** The write path still populates `extra` — it is the correct
//! shape if the substrate ever exposes the field, and it costs nothing — but
//! nothing here depends on it. What the boundary *can* rely on is the set of
//! ids Director itself completed, passed as `trusted_done_ids`; that is how
//! the verification rule holds, and that set lives in the adapter's own state
//! ([`crate::handoff::adapter`]), not in the substrate.

use serde_json::Value;

use director_domain::agent::{Agent, AgentStatus, Harness};
use director_domain::ids::{AgentId, MachineId, SessionId, TaskId};
use director_domain::session::{AgentSession, SessionEnd, SessionStatus};
use director_domain::task::{Complexity, ExpectedOutput, Priority, Task, TaskStatus};

use crate::handoff::wire::{AgentRecord, Schedule, TaskData};

/// Key under which Director's own task status is stashed in `TaskData.extra`
/// when it has no substrate equivalent.
pub const DIRECTOR_STATUS_KEY: &str = "director_status";

/// Key marking that a transition to `done` was Director's own decision (i.e.
/// verification passed), as opposed to an agent's self-report. Its presence is
/// the *only* thing that lets a substrate `done` become Director `Done`.
pub const DIRECTOR_VERIFIED_KEY: &str = "director_verified";

/// Key for Director's `Priority::Critical`, which the substrate cannot express.
pub const DIRECTOR_PRIORITY_KEY: &str = "director_priority";

/// Key for the capability list the substrate has no field for.
pub const DIRECTOR_CAPABILITIES_KEY: &str = "director_capabilities";

/// Key for Director's `Complexity`, which the substrate does not model.
pub const DIRECTOR_COMPLEXITY_KEY: &str = "director_complexity";

/// Convert a substrate task into Director's task.
///
/// `trusted_done_ids` is the set of task ids whose `done` state Director itself
/// produced. Everything else reported `done` by the substrate is an agent
/// self-report and becomes `VerificationPending`.
pub fn task_from_wire(
    data: &TaskData,
    trusted_done_ids: &std::collections::HashSet<String>,
) -> Task {
    let status = status_from_wire(
        &data.status,
        &data.extra,
        data.id.as_str(),
        trusted_done_ids,
    );
    let priority = priority_from_wire(&data.priority, &data.extra);
    let complexity = data
        .extra
        .get(DIRECTOR_COMPLEXITY_KEY)
        .and_then(Value::as_str)
        .map(complexity_from_wire)
        .unwrap_or(Complexity::Unknown);

    let required_capabilities = data
        .extra
        .get(DIRECTOR_CAPABILITIES_KEY)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(capability_from_wire)
                .collect()
        })
        .unwrap_or_default();

    let mut task = Task::new(
        TaskId::from_string(data.id.clone()),
        data.title.clone(),
        objective_of(data),
    );
    task.status = status;
    task.priority = priority;
    task.complexity = complexity;
    task.required_capabilities = required_capabilities;
    task.dependencies = data
        .dependencies
        .iter()
        .map(|d| TaskId::from_string(d.clone()))
        .collect();
    task.scope_paths = data.scope_paths.clone();
    task.expected_outputs = data
        .done_criteria
        .iter()
        .map(|c| ExpectedOutput {
            criterion: c.item.clone(),
            check: None,
        })
        .collect();
    task.created_at = parse_timestamp(&data.created_at).unwrap_or_else(chrono::Utc::now);
    task.updated_at = parse_timestamp(&data.updated_at).unwrap_or_else(chrono::Utc::now);
    task
}

/// The substrate has no objective field; the notes field is the closest honest
/// place a longer statement of intent can live, and the done criteria say what
/// "done" means. Prefer those, falling back to the title.
fn objective_of(data: &TaskData) -> String {
    if let Some(notes) = data.notes.as_deref().filter(|n| !n.trim().is_empty()) {
        return notes.to_string();
    }
    data.title.clone()
}

/// Convert Director's task into the substrate's wire shape.
///
/// Director-only states are preserved in `extra` so the round trip is lossless,
/// while the substrate's own `status` is set to the nearest state it can
/// actually express.
pub fn task_to_wire(task: &Task) -> TaskData {
    let (substrate_status, extra_status) = status_to_wire(task.status);
    let (substrate_priority, extra_priority) = priority_to_wire(task.priority);

    let mut extra = std::collections::HashMap::new();
    if let Some(s) = extra_status {
        extra.insert(DIRECTOR_STATUS_KEY.into(), Value::String(s));
    }
    if let Some(p) = extra_priority {
        extra.insert(DIRECTOR_PRIORITY_KEY.into(), Value::String(p));
    }
    if !matches!(task.complexity, Complexity::Unknown) {
        extra.insert(
            DIRECTOR_COMPLEXITY_KEY.into(),
            Value::String(complexity_to_wire(task.complexity).to_string()),
        );
    }
    if !task.required_capabilities.is_empty() {
        extra.insert(
            DIRECTOR_CAPABILITIES_KEY.into(),
            Value::Array(
                task.required_capabilities
                    .iter()
                    .map(|c| Value::String(capability_to_wire(c)))
                    .collect(),
            ),
        );
    }
    // A Director-confirmed completion carries the marker that lets a later read
    // trust the substrate's `done`. Its absence is what makes an agent's
    // self-reported `done` come back as `VerificationPending`.
    if task.status == TaskStatus::Done {
        extra.insert(DIRECTOR_VERIFIED_KEY.into(), Value::Bool(true));
    }

    TaskData {
        id: task.id.to_string(),
        title: task.title.clone(),
        status: substrate_status,
        notes: if task.objective != task.title {
            Some(task.objective.clone())
        } else {
            None
        },
        priority: substrate_priority,
        created_at: task.created_at.to_rfc3339(),
        updated_at: task.updated_at.to_rfc3339(),
        completed_at: if task.status.is_terminal() {
            Some(task.updated_at.to_rfc3339())
        } else {
            None
        },
        labels: vec![],
        links: vec![],
        task_links: vec![],
        done_criteria: task
            .expected_outputs
            .iter()
            .map(|o| crate::handoff::wire::DoneCriterion {
                item: o.criterion.clone(),
                // Never checked by Director: ticking this is an agent
                // self-report, and Director does not treat it as evidence.
                checked: false,
            })
            .collect(),
        schedule: None,
        dependencies: task.dependencies.iter().map(|d| d.to_string()).collect(),
        dependents: None,
        order: None,
        assignee: None,
        lock: None,
        scope_paths: task.scope_paths.clone(),
        extra,
    }
}

/// Read a substrate status as Director's, applying the verification rule.
pub fn status_from_wire(
    substrate: &str,
    extra: &std::collections::HashMap<String, Value>,
    task_id: &str,
    trusted_done_ids: &std::collections::HashSet<String>,
) -> TaskStatus {
    // A Director-only status always wins over the substrate fallback.
    if let Some(director_status) = extra.get(DIRECTOR_STATUS_KEY).and_then(Value::as_str) {
        return status_from_director_string(director_status);
    }

    match substrate {
        "todo" => TaskStatus::Todo,
        "in_progress" => TaskStatus::InProgress,
        "blocked" => TaskStatus::Blocked,
        // The substrate's notion of review has no Director equivalent; treat
        // it as work awaiting a verdict rather than completed.
        "review" => TaskStatus::VerificationPending,
        "done" => {
            // Only Director's own completions are Done. Any other `done` is an
            // agent self-report and must wait for verification.
            let verified = extra
                .get(DIRECTOR_VERIFIED_KEY)
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if verified || trusted_done_ids.contains(task_id) {
                TaskStatus::Done
            } else {
                TaskStatus::VerificationPending
            }
        }
        "skipped" => TaskStatus::Cancelled,
        // An unknown substrate status is not silently coerced: it becomes a
        // pending state that forces a human or the planner to look.
        _ => TaskStatus::VerificationPending,
    }
}

/// Map Director's status to the substrate's closest state, reporting which
/// Director value had to be stashed in `extra` because the substrate cannot
/// express it.
pub fn status_to_wire(status: TaskStatus) -> (String, Option<String>) {
    match status {
        TaskStatus::Todo => ("todo".to_string(), None),
        TaskStatus::InProgress => ("in_progress".to_string(), None),
        TaskStatus::Blocked => ("blocked".to_string(), None),
        // Director's Done is written together with the verified marker.
        TaskStatus::Done => ("done".to_string(), None),
        TaskStatus::Backlog => ("todo".to_string(), Some("backlog".to_string())),
        TaskStatus::VerificationPending => (
            "in_progress".to_string(),
            Some("verification_pending".to_string()),
        ),
        // The substrate has no failure state. `done` is the nearest honest
        // terminal state; the real status is preserved in `extra`.
        TaskStatus::Failed => ("done".to_string(), Some("failed".to_string())),
        TaskStatus::Cancelled => ("done".to_string(), Some("cancelled".to_string())),
    }
}

fn status_from_director_string(s: &str) -> TaskStatus {
    match s {
        "backlog" => TaskStatus::Backlog,
        "todo" => TaskStatus::Todo,
        "in_progress" => TaskStatus::InProgress,
        "blocked" => TaskStatus::Blocked,
        "verification_pending" => TaskStatus::VerificationPending,
        "done" => TaskStatus::Done,
        "failed" => TaskStatus::Failed,
        "cancelled" => TaskStatus::Cancelled,
        _ => TaskStatus::VerificationPending,
    }
}

fn priority_from_wire(
    substrate: &Option<String>,
    extra: &std::collections::HashMap<String, Value>,
) -> Option<Priority> {
    // Director's Critical is recovered before the substrate value is consulted.
    if let Some(p) = extra.get(DIRECTOR_PRIORITY_KEY).and_then(Value::as_str) {
        if p == "critical" {
            return Some(Priority::Critical);
        }
    }
    substrate.as_deref().map(|p| match p {
        "high" => Priority::High,
        "medium" => Priority::Medium,
        "low" => Priority::Low,
        _ => Priority::Medium,
    })
}

fn priority_to_wire(priority: Option<Priority>) -> (Option<String>, Option<String>) {
    match priority {
        None => (None, None),
        Some(Priority::Critical) => (Some("high".to_string()), Some("critical".to_string())),
        Some(Priority::High) => (Some("high".to_string()), None),
        Some(Priority::Medium) => (Some("medium".to_string()), None),
        Some(Priority::Low) => (Some("low".to_string()), None),
    }
}

fn complexity_from_wire(s: &str) -> Complexity {
    match s {
        "trivial" => Complexity::Trivial,
        "small" => Complexity::Small,
        "medium" => Complexity::Medium,
        "large" => Complexity::Large,
        _ => Complexity::Unknown,
    }
}

fn complexity_to_wire(c: Complexity) -> &'static str {
    match c {
        Complexity::Trivial => "trivial",
        Complexity::Small => "small",
        Complexity::Medium => "medium",
        Complexity::Large => "large",
        Complexity::Unknown => "unknown",
    }
}

fn capability_from_wire(s: &str) -> director_domain::capability::Capability {
    use director_domain::capability::Capability;
    match s {
        "coding" => Capability::Coding,
        "testing" => Capability::Testing,
        "reviewing" => Capability::Reviewing,
        "planning" => Capability::Planning,
        "documentation" => Capability::Documentation,
        "research" => Capability::Research,
        "frontend" => Capability::Frontend,
        "backend" => Capability::Backend,
        "database" => Capability::Database,
        "devops" => Capability::DevOps,
        "security" => Capability::Security,
        "human" => Capability::Human,
        other => Capability::Custom(other.to_string()),
    }
}

fn capability_to_wire(c: &director_domain::capability::Capability) -> String {
    use director_domain::capability::Capability;
    match c {
        Capability::Coding => "coding",
        Capability::Testing => "testing",
        Capability::Reviewing => "reviewing",
        Capability::Planning => "planning",
        Capability::Documentation => "documentation",
        Capability::Research => "research",
        Capability::Frontend => "frontend",
        Capability::Backend => "backend",
        Capability::Database => "database",
        Capability::DevOps => "devops",
        Capability::Security => "security",
        Capability::Human => "human",
        Capability::Custom(name) => return name.clone(),
    }
    .to_string()
}

/// Map the substrate's agent record to Director's.
///
/// The substrate's statuses are derived from heartbeat age: `Active` covers both
/// of Director's `Available` and `Busy`. Director resolves the ambiguity from
/// whether the agent holds claimed tasks, which the substrate reports only when
/// asked — so the caller passes it in.
pub fn agent_from_wire(record: &AgentRecord) -> Agent {
    let mut agent = Agent::register(
        AgentId::from_string(record.agent_id.clone()),
        record.agent_id.clone(),
        Harness::Other("handoff-mcp".to_string()),
        MachineId::from_string(machine_id_for(&record.worktree)),
        vec![],
    );
    agent.status = agent_status_from_wire(&record.status, !record.claimed_tasks.is_empty());
    agent.registered_at = parse_timestamp(&record.registered_at).unwrap_or(agent.registered_at);
    agent.last_seen = parse_timestamp(&record.last_heartbeat).unwrap_or(agent.last_seen);
    if let Some(session) = record.session_id.as_deref() {
        agent.current_task = record
            .claimed_tasks
            .first()
            .map(|t| TaskId::from_string(t.clone()));
        // Session association is recorded on the session side; the agent keeps
        // its denormalized task pointer only.
        let _ = session;
    }
    agent
}

/// A stable machine id derived from the worktree path the agent registered in.
fn machine_id_for(worktree: &str) -> String {
    // Deterministic and human-readable: two agents in the same worktree are on
    // the same machine as far as the substrate can tell.
    let slug: String = worktree
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("MACH-{slug}")
}

fn agent_status_from_wire(substrate: &str, holds_tasks: bool) -> AgentStatus {
    match substrate {
        // The substrate's Active spans Director's Available and Busy.
        "active" => {
            if holds_tasks {
                AgentStatus::Busy
            } else {
                AgentStatus::Available
            }
        }
        "stale" => AgentStatus::Stale,
        "disconnected" => AgentStatus::Disconnected,
        _ => AgentStatus::Offline,
    }
}

/// Map a substrate session summary to Director's session.
///
/// Ownership, lineage, and working directory all come from the summary itself
/// — the substrate's `handoff_list_sessions` carries them whenever the
/// underlying session record has them. When it does not, the caller falls back
/// to [`unknown_agent_id`] rather than inventing an owner: Director would
/// rather show an unattributed session than attribute it to the wrong agent.
pub fn session_from_wire(summary: &crate::handoff::wire::SessionSummary) -> AgentSession {
    let agent_id = summary
        .agent_id
        .clone()
        .unwrap_or_else(|| unknown_agent_id().to_string());
    let mut session = AgentSession::start(
        SessionId::from_string(summary.id.clone()),
        AgentId::from_string(agent_id),
        MachineId::from_string(machine_id_for(
            summary.worktree.as_deref().unwrap_or("unknown"),
        )),
        None,
    );
    session.parent_session_id = summary
        .parent_session_id
        .clone()
        .map(SessionId::from_string);
    session.workdir = summary.worktree.clone();
    session.started_at = summary
        .started_at
        .as_deref()
        .and_then(parse_timestamp)
        .unwrap_or(session.started_at);
    session.status = match summary.status.as_str() {
        "open" => SessionStatus::Open,
        "active" => SessionStatus::Active,
        "paused" => SessionStatus::Paused,
        _ => SessionStatus::Closed,
    };
    session.ended_at = summary.ended_at.as_deref().and_then(parse_timestamp);
    session.end = session
        .ended_at
        .map(|_| session_end_from_wire(&summary.status));
    session
}

/// The agent id used when the substrate gives us no owner for a session.
pub fn unknown_agent_id() -> &'static str {
    "AGENT-unknown"
}

fn session_end_from_wire(substrate_status: &str) -> SessionEnd {
    match substrate_status {
        // The substrate distinguishes closed sessions only by the fact of
        // closing; Director's richer reasons are resolved later from whether
        // the close was expected.
        "closed" => SessionEnd::Clean,
        "paused" => SessionEnd::ContextExhausted,
        _ => SessionEnd::Forked,
    }
}

/// Parse the substrate's RFC3339 timestamps, tolerating the offset forms it
/// emits (`+00:00`, and fractional seconds beyond chrono's nanosecond bound).
pub fn parse_timestamp(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .or_else(|| {
            // The server emits 9 fractional digits (`...427288500+00:00`);
            // chrono accepts these directly, but if it ever does not, truncate
            // to the seconds boundary rather than losing the timestamp.
            s.split('.')
                .next()
                .and_then(|secs| {
                    chrono::NaiveDateTime::parse_from_str(secs, "%Y-%m-%dT%H:%M:%S").ok()
                })
                .map(|naive| naive.and_utc())
        })
}

// `Schedule` is read but never written; reference it so the import stays honest
// about which parts of the wire model Director uses.
#[allow(unused_imports)]
use Schedule as _ReadSchedule;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn wire_with_status(status: &str) -> TaskData {
        TaskData {
            id: "AUTH-42".into(),
            title: "Auth".into(),
            status: status.into(),
            notes: None,
            priority: None,
            created_at: "2026-09-24T19:34:17.427288500+00:00".into(),
            updated_at: "2026-09-24T19:34:17.427288500+00:00".into(),
            completed_at: None,
            labels: vec![],
            links: vec![],
            task_links: vec![],
            done_criteria: vec![],
            schedule: None,
            dependencies: vec![],
            dependents: None,
            order: None,
            assignee: None,
            lock: None,
            scope_paths: vec![],
            extra: HashMap::new(),
        }
    }

    #[test]
    fn a_substrate_done_without_verification_is_not_done() {
        // The acceptance criterion, at the boundary: an agent reporting done
        // must not complete the task in Director's model.
        let data = wire_with_status("done");
        let trusted = std::collections::HashSet::new();
        let task = task_from_wire(&data, &trusted);
        assert_eq!(task.status, TaskStatus::VerificationPending);
    }

    #[test]
    fn a_director_marked_done_round_trips_as_done() {
        // When Director itself completed the task, the marker is on the record
        // and the status comes back as Done.
        let mut task = Task::new(TaskId::from_string("AUTH-42"), "Auth", "objective");
        task.status = TaskStatus::Done;
        let wire = task_to_wire(&task);
        assert_eq!(wire.status, "done");
        assert_eq!(
            wire.extra.get(DIRECTOR_VERIFIED_KEY),
            Some(&Value::Bool(true))
        );

        let trusted: std::collections::HashSet<String> =
            ["AUTH-42".to_string()].into_iter().collect();
        let back = task_from_wire(&wire, &trusted);
        assert_eq!(back.status, TaskStatus::Done);
    }

    #[test]
    fn shared_statuses_map_directly() {
        let trusted = std::collections::HashSet::new();
        assert_eq!(
            task_from_wire(&wire_with_status("todo"), &trusted).status,
            TaskStatus::Todo
        );
        assert_eq!(
            task_from_wire(&wire_with_status("in_progress"), &trusted).status,
            TaskStatus::InProgress
        );
        assert_eq!(
            task_from_wire(&wire_with_status("blocked"), &trusted).status,
            TaskStatus::Blocked
        );
    }

    #[test]
    fn substrate_skipped_is_director_cancelled() {
        let trusted = std::collections::HashSet::new();
        assert_eq!(
            task_from_wire(&wire_with_status("skipped"), &trusted).status,
            TaskStatus::Cancelled
        );
    }

    #[test]
    fn substrate_review_awaits_verification() {
        let trusted = std::collections::HashSet::new();
        assert_eq!(
            task_from_wire(&wire_with_status("review"), &trusted).status,
            TaskStatus::VerificationPending
        );
    }

    #[test]
    fn director_only_statuses_survive_a_round_trip() {
        for status in [
            TaskStatus::Backlog,
            TaskStatus::VerificationPending,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            let mut task = Task::new(TaskId::from_string("AUTH-42"), "Auth", "objective");
            task.status = status;
            let wire = task_to_wire(&task);

            // The real status is preserved in extra, and readable back.
            let director_status = wire
                .extra
                .get(DIRECTOR_STATUS_KEY)
                .and_then(Value::as_str)
                .unwrap_or("missing");
            let restored = status_from_director_string(director_status);
            assert_eq!(restored, status, "round trip lost status {status:?}");
        }
    }

    #[test]
    fn critical_priority_round_trips_through_extra() {
        let mut task = Task::new(TaskId::from_string("AUTH-42"), "Auth", "objective");
        task.priority = Some(Priority::Critical);
        let wire = task_to_wire(&task);

        // Down-graded on the wire, because the substrate cannot express it.
        assert_eq!(wire.priority.as_deref(), Some("high"));
        // ...and restored on read.
        let task = task_from_wire(&wire, &std::collections::HashSet::new());
        assert_eq!(task.priority, Some(Priority::Critical));
    }

    #[test]
    fn ordinary_priorities_map_directly() {
        let mut task = Task::new(TaskId::from_string("AUTH-42"), "Auth", "objective");
        task.priority = Some(Priority::Low);
        assert_eq!(task_to_wire(&task).priority.as_deref(), Some("low"));

        let mut extra = HashMap::new();
        let wire = wire_with_status("todo");
        let mut wire = wire;
        wire.priority = Some("medium".into());
        wire.extra = std::mem::take(&mut extra);
        let task = task_from_wire(&wire, &std::collections::HashSet::new());
        assert_eq!(task.priority, Some(Priority::Medium));
    }

    #[test]
    fn dependencies_and_scope_round_trip() {
        let mut task = Task::new(TaskId::from_string("AUTH-42"), "Auth", "objective");
        task.dependencies = vec![TaskId::from_string("DB-1"), TaskId::from_string("DB-2")];
        task.scope_paths = vec!["src/auth/".into()];
        let wire = task_to_wire(&task);
        assert_eq!(wire.dependencies, vec!["DB-1", "DB-2"]);
        assert_eq!(wire.scope_paths, vec!["src/auth/"]);
    }

    #[test]
    fn expected_outputs_become_criteria_but_are_never_checked() {
        let mut task = Task::new(TaskId::from_string("AUTH-42"), "Auth", "objective");
        task.expected_outputs.push(ExpectedOutput {
            criterion: "login returns 200".into(),
            check: None,
        });
        let wire = task_to_wire(&task);
        assert_eq!(wire.done_criteria.len(), 1);
        // Never ticked: a checked criterion is an agent self-report, not
        // evidence Director would act on.
        assert!(!wire.done_criteria[0].checked);
    }

    #[test]
    fn an_unknown_substrate_status_does_not_silently_become_done() {
        let trusted = std::collections::HashSet::new();
        let task = task_from_wire(&wire_with_status("not_a_real_status"), &trusted);
        assert_eq!(task.status, TaskStatus::VerificationPending);
    }

    #[test]
    fn timestamp_parsing_handles_the_servers_form() {
        let parsed = parse_timestamp("2026-09-24T19:34:17.427288500+00:00");
        assert!(parsed.is_some());
        assert_eq!(parsed.unwrap().format("%Y-%m-%d").to_string(), "2026-09-24");
    }

    #[test]
    fn an_active_agent_with_tasks_is_busy_not_available() {
        let mut record = AgentRecord {
            agent_id: "AGENT-1".into(),
            session_id: None,
            worktree: "C:/repo".into(),
            branch: None,
            status: "active".into(),
            registered_at: "2026-09-24T19:00:00+00:00".into(),
            last_heartbeat: "2026-09-24T20:00:00+00:00".into(),
            claimed_tasks: vec![],
        };
        assert_eq!(agent_from_wire(&record).status, AgentStatus::Available);
        record.claimed_tasks = vec!["AUTH-42".into()];
        assert_eq!(agent_from_wire(&record).status, AgentStatus::Busy);
    }

    #[test]
    fn a_stale_or_disconnected_agent_maps_through() {
        let mut record = AgentRecord {
            agent_id: "AGENT-1".into(),
            session_id: None,
            worktree: "C:/repo".into(),
            branch: None,
            status: "stale".into(),
            registered_at: "2026-09-24T19:00:00+00:00".into(),
            last_heartbeat: "2026-09-24T20:00:00+00:00".into(),
            claimed_tasks: vec![],
        };
        assert_eq!(agent_from_wire(&record).status, AgentStatus::Stale);
        record.status = "disconnected".into();
        assert_eq!(agent_from_wire(&record).status, AgentStatus::Disconnected);
    }

    #[test]
    fn a_session_summary_carries_owner_lineage_and_workdir() {
        let summary = crate::handoff::wire::SessionSummary {
            id: "SESS-2".into(),
            status: "closed".into(),
            summary: "shipped the login page".into(),
            started_at: Some("2026-09-24T19:00:00+00:00".into()),
            ended_at: Some("2026-09-24T21:00:00+00:00".into()),
            branch: Some("feat/login".into()),
            commit: Some("abc123".into()),
            decisions_count: 3,
            checklist_progress: "2/2".into(),
            agent_id: Some("AGENT-7".into()),
            parent_session_id: Some("SESS-1".into()),
            worktree: Some("C:/repo/worktrees/login".into()),
        };

        let session = session_from_wire(&summary);
        assert_eq!(session.id, SessionId::from_string("SESS-2"));
        assert_eq!(session.agent_id, AgentId::from_string("AGENT-7"));
        assert_eq!(
            session.parent_session_id,
            Some(SessionId::from_string("SESS-1"))
        );
        assert_eq!(session.status, SessionStatus::Closed);
        assert_eq!(session.end, Some(SessionEnd::Clean));
        assert_eq!(session.workdir.as_deref(), Some("C:/repo/worktrees/login"));
    }

    #[test]
    fn an_unattributed_session_falls_back_to_unknown_not_to_a_guess() {
        let summary = crate::handoff::wire::SessionSummary {
            id: "SESS-9".into(),
            status: "closed".into(),
            summary: String::new(),
            started_at: None,
            ended_at: None,
            branch: None,
            commit: None,
            decisions_count: 0,
            checklist_progress: String::new(),
            agent_id: None,
            parent_session_id: None,
            worktree: None,
        };

        let session = session_from_wire(&summary);
        assert_eq!(session.agent_id, AgentId::from_string(unknown_agent_id()));
        assert!(session.parent_session_id.is_none());
        assert!(
            session.ended_at.is_none(),
            "a session that never ended has no end reason"
        );
    }
}
