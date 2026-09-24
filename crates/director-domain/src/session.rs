//! [AgentSession]: one concrete invocation of one agent on one machine.
//!
//! A session is the *evidence trail*, not the work. When a session dies, its
//! observations and the checkpoint it produced are what let the next session
//! continue. Director keeps sessions clearly distinct from tasks: a task may
//! accumulate many sessions as it passes between agents.

use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, MachineId, SessionId, TaskId};

/// How a session ended, if it did. This is what tells recovery (Phase 11)
/// whether it is recovering from a clean close or a disappearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEnd {
    /// The agent closed out properly — for handoff-mcp, `handoff_save_context`
    /// with `session_status: "closed"`; for ai-memory, a true `SessionEnd`
    /// lifecycle hook.
    Clean,
    /// Director noticed the heartbeat stop before a clean close.
    Vanished,
    /// Director or an operator terminated it.
    Terminated,
    /// The context window filled and the session compacted or was rotated
    /// (Phase 17). Not a failure: Director checkpoints through this.
    ContextExhausted,
    /// The session forked from another; both continue.
    Forked,
}

/// Lifecycle state of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Registered, not yet doing work.
    Open,
    /// Actively working.
    Active,
    /// Deliberately paused; resumable.
    Paused,
    /// Ended, by any means.
    Closed,
}

impl SessionStatus {
    /// True if the session can still produce observations.
    pub fn is_live(self) -> bool {
        matches!(
            self,
            SessionStatus::Open | SessionStatus::Active | SessionStatus::Paused
        )
    }
}

/// One invocation of an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    /// This session's identifier.
    pub id: SessionId,
    /// The agent running it.
    pub agent_id: AgentId,
    /// The machine it ran on.
    pub machine_id: MachineId,
    /// Task this session was launched against, if any. A session may do work
    /// outside a task (exploration, a question); that is fine, it just is not
    /// part of any task's history.
    pub task_id: Option<TaskId>,
    /// Lifecycle state.
    pub status: SessionStatus,
    /// If this session was forked from another, its parent. Mirrors
    /// handoff-mcp's `parent_session_id` lineage.
    pub parent_session_id: Option<SessionId>,
    /// When the session began.
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// When it ended, if it has.
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    /// How it ended, if it has.
    pub end: Option<SessionEnd>,
    /// Working directory the session ran in, so Director can tell a primary
    /// checkout from a worktree.
    pub workdir: Option<String>,
}

impl AgentSession {
    /// Start a new live session.
    pub fn start(
        id: SessionId,
        agent_id: AgentId,
        machine_id: MachineId,
        task_id: Option<TaskId>,
    ) -> Self {
        AgentSession {
            id,
            agent_id,
            machine_id,
            task_id,
            status: SessionStatus::Active,
            parent_session_id: None,
            started_at: chrono::Utc::now(),
            ended_at: None,
            end: None,
            workdir: None,
        }
    }

    /// Fork this session into a new one, recording lineage.
    pub fn fork(&self, new_id: SessionId) -> AgentSession {
        let mut child = AgentSession::start(
            new_id,
            self.agent_id.clone(),
            self.machine_id.clone(),
            self.task_id.clone(),
        );
        child.parent_session_id = Some(self.id.clone());
        child.workdir = self.workdir.clone();
        child
    }

    /// Close the session with a specific end reason.
    pub fn close(&mut self, end: SessionEnd) {
        self.status = SessionStatus::Closed;
        self.end = Some(end);
        self.ended_at = Some(chrono::Utc::now());
    }

    /// True if this session ended without a clean close — the signal that
    /// recovery should treat its last checkpoint as possibly incomplete.
    pub fn ended_uncleanly(&self) -> bool {
        matches!(
            self.end,
            Some(SessionEnd::Vanished) | Some(SessionEnd::Terminated)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> AgentSession {
        AgentSession::start(
            SessionId::from_string("SESS-1"),
            AgentId::from_string("AGENT-1"),
            MachineId::from_string("MACH-a"),
            Some(TaskId::from_string("AUTH-42")),
        )
    }

    #[test]
    fn a_started_session_is_active() {
        let s = session();
        assert_eq!(s.status, SessionStatus::Active);
        assert!(s.status.is_live());
        assert!(s.ended_at.is_none());
    }

    #[test]
    fn a_clean_close_records_the_reason() {
        let mut s = session();
        s.close(SessionEnd::Clean);
        assert_eq!(s.status, SessionStatus::Closed);
        assert!(!s.status.is_live());
        assert_eq!(s.end, Some(SessionEnd::Clean));
        assert!(!s.ended_uncleanly());
    }

    #[test]
    fn a_vanished_session_is_flagged_unclean() {
        let mut s = session();
        s.close(SessionEnd::Vanished);
        assert!(s.ended_uncleanly());
    }

    #[test]
    fn context_exhaustion_is_not_an_unclean_end() {
        // Phase 17: compaction is a planned event, not a crash.
        let mut s = session();
        s.close(SessionEnd::ContextExhausted);
        assert!(!s.ended_uncleanly());
    }

    #[test]
    fn fork_records_parent_lineage_and_inherits_scope() {
        let mut parent = session();
        parent.workdir = Some("/repo".into());
        let child = parent.fork(SessionId::from_string("SESS-2"));

        assert_eq!(
            child.parent_session_id,
            Some(SessionId::from_string("SESS-1"))
        );
        assert_eq!(child.workdir, Some("/repo".into()));
        assert_eq!(child.task_id, parent.task_id);
        assert_ne!(child.id, parent.id);
    }

    #[test]
    fn session_round_trips_through_serde() {
        let s = session();
        let json = serde_json::to_string(&s).unwrap();
        let back: AgentSession = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }
}
