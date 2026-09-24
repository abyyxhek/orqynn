//! [Handoff] — Director's own transfer of a task between agents (Phase 8/12).
//!
//! This is deliberately **not** a reimplementation of either substrate's
//! handoff. ai-memory's `Handoff` is a session-scoped summary produced on
//! `SessionEnd`; handoff-mcp's is `handoff_notes` in a session document.
//! Director's handoff is task-scoped and **claim-once**: it is addressed to a
//! specific next agent, and accepting it consumes it, so two agents can never
//! both pick up the same task from the same transfer.
//!
//! The difference that matters: a Director handoff is always about *a task
//! continuing*, never about *a session ending*. A session ending without a
//! handoff is a crash, and recovery (Phase 11) builds the handoff retroactively
//! from the last checkpoint.

use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, HandoffId, SessionId, TaskId};

/// Lifecycle state of a handoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffState {
    /// Waiting to be accepted by the intended agent.
    Open,
    /// Accepted; the receiving agent owns the task now.
    Accepted,
    /// Nobody claimed it before it stopped being relevant.
    Expired,
}

impl HandoffState {
    /// True if the handoff is still waiting to be picked up.
    pub fn is_open(self) -> bool {
        matches!(self, HandoffState::Open)
    }

    /// True if the handoff was consumed by an agent.
    pub fn was_accepted(self) -> bool {
        matches!(self, HandoffState::Accepted)
    }
}

/// A task transfer from one agent to another, with everything the receiver
/// needs to continue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    /// This handoff's identifier.
    pub id: HandoffId,
    /// The task being transferred.
    pub task_id: TaskId,
    /// Agent handing the task off.
    pub from_agent: AgentId,
    /// Session that produced the handoff, for attribution.
    pub from_session: Option<SessionId>,
    /// Intended recipient. Claim-once: only this agent can accept.
    pub to_agent: AgentId,
    /// Open, accepted, or expired.
    pub state: HandoffState,
    /// What the receiver needs to know, in short.
    pub summary: String,
    /// What the receiver should not have to re-derive.
    pub open_questions: Vec<String>,
    /// Concrete next steps, ordered.
    pub next_steps: Vec<String>,
    /// Files the outgoing agent touched, so the receiver knows where to look.
    pub files_touched: Vec<String>,
    /// When the handoff was opened.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When it was accepted, if it was.
    pub accepted_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The session that accepted it, if any.
    pub accepted_by_session: Option<SessionId>,
}

impl Handoff {
    /// Open a handoff addressed to `to_agent`.
    pub fn open(
        id: HandoffId,
        task_id: TaskId,
        from_agent: AgentId,
        to_agent: AgentId,
        summary: impl Into<String>,
    ) -> Self {
        Handoff {
            id,
            task_id,
            from_agent,
            from_session: None,
            to_agent,
            state: HandoffState::Open,
            summary: summary.into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            created_at: chrono::Utc::now(),
            accepted_at: None,
            accepted_by_session: None,
        }
    }

    /// Record the session the handoff was produced in.
    pub fn from(mut self, session_id: SessionId) -> Self {
        self.from_session = Some(session_id);
        self
    }

    /// Add a next step the receiver should take.
    pub fn then(mut self, step: impl Into<String>) -> Self {
        self.next_steps.push(step.into());
        self
    }

    /// Add an open question the receiver should be aware of.
    pub fn asking(mut self, question: impl Into<String>) -> Self {
        self.open_questions.push(question.into());
        self
    }

    /// Note a file the outgoing agent touched.
    pub fn touched(mut self, path: impl Into<String>) -> Self {
        self.files_touched.push(path.into());
        self
    }

    /// Accept the handoff on behalf of the intended agent.
    ///
    /// Returns `Err` if the caller is not the intended recipient — this is the
    /// claim-once guard. A handoff accepted by the wrong agent is a bug worth
    /// failing loudly on rather than silently working around.
    pub fn accept(&mut self, acceptor: AgentId, session_id: SessionId) -> Result<(), HandoffError> {
        if self.state != HandoffState::Open {
            return Err(HandoffError::AlreadyConsumed);
        }
        if acceptor != self.to_agent {
            return Err(HandoffError::WrongAgent);
        }
        self.state = HandoffState::Accepted;
        self.accepted_at = Some(chrono::Utc::now());
        self.accepted_by_session = Some(session_id);
        Ok(())
    }

    /// Expire an unclaimed handoff.
    pub fn expire(&mut self) {
        self.state = HandoffState::Expired;
    }
}

/// Why a handoff could not be accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HandoffError {
    /// The handoff was already accepted or expired.
    #[error("handoff is no longer open")]
    AlreadyConsumed,
    /// A different agent tried to accept a handoff addressed to someone else.
    #[error("handoff is addressed to a different agent")]
    WrongAgent,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handoff() -> Handoff {
        Handoff::open(
            HandoffId::from_string("HOF-1"),
            TaskId::from_string("AUTH-42"),
            AgentId::from_string("AGENT-claude"),
            AgentId::from_string("AGENT-codex"),
            "auth mostly done, tests outstanding",
        )
    }

    #[test]
    fn a_new_handoff_is_open() {
        let h = handoff();
        assert!(h.state.is_open());
        assert!(!h.state.was_accepted());
        assert!(h.accepted_at.is_none());
    }

    #[test]
    fn the_intended_agent_can_accept() {
        let mut h = handoff();
        h.accept(
            AgentId::from_string("AGENT-codex"),
            SessionId::from_string("SESS-2"),
        )
        .unwrap();

        assert!(h.state.was_accepted());
        assert_eq!(
            h.accepted_by_session,
            Some(SessionId::from_string("SESS-2"))
        );
    }

    #[test]
    fn another_agent_cannot_accept() {
        // The claim-once guard: a third agent must not be able to seize a
        // handoff addressed elsewhere.
        let mut h = handoff();
        let err = h
            .accept(
                AgentId::from_string("AGENT-deepseek"),
                SessionId::from_string("SESS-3"),
            )
            .unwrap_err();
        assert_eq!(err, HandoffError::WrongAgent);
        assert!(h.state.is_open());
    }

    #[test]
    fn a_handoff_cannot_be_accepted_twice() {
        let mut h = handoff();
        h.accept(
            AgentId::from_string("AGENT-codex"),
            SessionId::from_string("SESS-2"),
        )
        .unwrap();
        let err = h
            .accept(
                AgentId::from_string("AGENT-codex"),
                SessionId::from_string("SESS-2"),
            )
            .unwrap_err();
        assert_eq!(err, HandoffError::AlreadyConsumed);
    }

    #[test]
    fn expiring_makes_a_handoff_unacceptable() {
        let mut h = handoff();
        h.expire();
        assert!(!h.state.is_open());
        assert!(h
            .accept(
                AgentId::from_string("AGENT-codex"),
                SessionId::from_string("SESS-2")
            )
            .is_err());
    }

    #[test]
    fn a_handoff_accumulates_next_steps_questions_and_files() {
        let h = handoff()
            .from(SessionId::from_string("SESS-1"))
            .then("write the login test")
            .asking("do we support SSO?")
            .touched("src/auth/login.rs");

        assert_eq!(h.from_session, Some(SessionId::from_string("SESS-1")));
        assert_eq!(h.next_steps, vec!["write the login test"]);
        assert_eq!(h.open_questions, vec!["do we support SSO?"]);
        assert_eq!(h.files_touched, vec!["src/auth/login.rs"]);
    }

    #[test]
    fn handoff_round_trips_through_serde() {
        let h = handoff().then("write tests");
        let json = serde_json::to_string(&h).unwrap();
        let back: Handoff = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);
    }
}
