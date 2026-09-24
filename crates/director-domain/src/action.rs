//! [Action] — one entry in recent context.
//!
//! Actions are the *evidence* side of the system. Where a task says what
//! should happen, an action records what actually did. Director collects these
//! from observations (Phase 6) rather than from agent self-report, which is
//! what makes [crate::context::RecentContext] a record of work rather than a
//! record of claims.

use serde::{Deserialize, Serialize};

use crate::ids::{ActionId, AgentId, SessionId, TaskId};

/// What kind of thing happened. Coarse on purpose: this drives formatting and
/// importance ranking, not behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// The user or planner stated a goal.
    Prompt,
    /// A shell command ran.
    Command,
    /// A tool was invoked (file write, search, edit).
    ToolCall,
    /// A file on disk changed.
    FileChange,
    /// Something failed.
    Error,
    /// A checkpoint was taken.
    Checkpoint,
    /// A free-text note worth remembering.
    Note,
}

impl std::fmt::Display for ActionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionKind::Prompt => f.write_str("prompt"),
            ActionKind::Command => f.write_str("command"),
            ActionKind::ToolCall => f.write_str("tool-call"),
            ActionKind::FileChange => f.write_str("file-change"),
            ActionKind::Error => f.write_str("error"),
            ActionKind::Checkpoint => f.write_str("checkpoint"),
            ActionKind::Note => f.write_str("note"),
        }
    }
}

impl ActionKind {
    /// Rough importance for compaction ordering. Higher survives compaction
    /// longer. Errors and checkpoints outrank routine tool calls because they
    /// are what the next agent most needs to know about.
    pub const fn importance(self) -> u32 {
        match self {
            ActionKind::Error => 5,
            ActionKind::Checkpoint => 4,
            ActionKind::Prompt => 3,
            ActionKind::Note => 2,
            ActionKind::Command | ActionKind::ToolCall | ActionKind::FileChange => 1,
        }
    }
}

/// One observed event, in a form compact enough to hand to a fresh agent.
///
/// `detail` is deliberately optional and unbounded-free: it is the place for a
/// command line or an error message, not for a transcript. Bounded sizing
/// happens in [crate::context::RecentContext].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    /// This action's stable identifier.
    pub id: ActionId,
    /// Task this action belongs to, if any. Actions outside a task are kept
    /// too — exploration is context.
    pub task_id: Option<TaskId>,
    /// The agent that performed it, if known.
    pub agent_id: Option<AgentId>,
    /// The session it happened in, if known.
    pub session_id: Option<SessionId>,
    /// What kind of thing happened.
    pub kind: ActionKind,
    /// One line: "ran `cargo test`", "edited src/auth.rs".
    pub summary: String,
    /// Longer payload: the command, the error text, the file list.
    pub detail: Option<String>,
    /// When it happened.
    pub at: chrono::DateTime<chrono::Utc>,
}

impl Action {
    /// Record an action at the current time.
    pub fn new(id: ActionId, kind: ActionKind, summary: impl Into<String>) -> Self {
        Action {
            id,
            task_id: None,
            agent_id: None,
            session_id: None,
            kind,
            summary: summary.into(),
            detail: None,
            at: chrono::Utc::now(),
        }
    }

    /// Attach this action to a task, returning it for chaining.
    pub fn for_task(mut self, task_id: TaskId) -> Self {
        self.task_id = Some(task_id);
        self
    }

    /// Record which agent and session performed the action.
    pub fn by(mut self, agent_id: AgentId, session_id: SessionId) -> Self {
        self.agent_id = Some(agent_id);
        self.session_id = Some(session_id);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_and_checkpoints_outrank_routine_work() {
        assert!(ActionKind::Error.importance() > ActionKind::ToolCall.importance());
        assert!(ActionKind::Checkpoint.importance() > ActionKind::Command.importance());
        assert!(ActionKind::Prompt.importance() > ActionKind::Note.importance());
    }

    #[test]
    fn an_action_can_be_scoped_after_construction() {
        let a = Action::new(
            ActionId::from_string("ACT-1"),
            ActionKind::Command,
            "ran cargo test",
        )
        .for_task(TaskId::from_string("AUTH-42"))
        .by(
            AgentId::from_string("AGENT-claude"),
            SessionId::from_string("SESS-1"),
        );

        assert_eq!(a.task_id, Some(TaskId::from_string("AUTH-42")));
        assert_eq!(a.agent_id, Some(AgentId::from_string("AGENT-claude")));
        assert!(a.detail.is_none());
    }

    #[test]
    fn action_round_trips_through_serde() {
        let a = Action::new(
            ActionId::from_string("ACT-1"),
            ActionKind::Error,
            "tests failed",
        )
        .for_task(TaskId::from_string("AUTH-42"));
        let json = serde_json::to_string(&a).unwrap();
        let back: Action = serde_json::from_str(&json).unwrap();
        assert_eq!(a, back);
    }
}
