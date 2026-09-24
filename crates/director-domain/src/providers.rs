//! The provider trait boundary (Phase 1).
//!
//! ## Why these traits exist
//!
//! Director's core depends **only** on these traits. Every substrate —
//! handoff-mcp over MCP, ai-memory over MCP, an in-memory fake for tests — is
//! a struct that implements them. The consequence is the single most important
//! structural property of the system:
//!
//! > Director's loop can be developed and tested with zero external processes,
//! > and a substrate can be swapped or upgraded without touching the loop.
//!
//! ## Rules every trait follows
//!
//! 1. **Methods are `async`**, because every real substrate is an I/O boundary
//!    (an MCP round-trip, a file, a database). A synchronous trait would make
//!    the loop lie about what it is doing.
//! 2. **Inputs and outputs are Director's own types**, defined in this crate.
//!    A trait that returned a substrate's struct would make the boundary
//!    fictional. Adapters do the translation; the trait never leaks it.
//! 3. **Each trait has an associated [`Error`](Provider::Error)**, so a
//!    substrate's failure vocabulary cannot become Director's.
//! 4. **Identity arguments are always Director ids**, never substrate ids.
//!    Adapters hold the identity translation.
//! 5. **Nothing here completes a task.** No trait has a "mark done" method that
//!    an agent's report can reach. Verification is a Director-owned engine
//!    (Phase 10), not a provider capability.
//!
//! ## What is *not* here
//!
//! Deliberately absent: checkpoint, plan, decision, blocker, verification, and
//! assignment storage. Those are Director-owned entities that live in
//! Director's own store (Phase 5). No substrate has them, and exposing them as
//! provider traits would invite a substrate to become authoritative over
//! Director's own state.

use std::path::Path;

use async_trait::async_trait;

use crate::agent::{Agent, AgentStatus};
use crate::handoff::Handoff;
use crate::ids::{AgentId, HandoffId, ProjectId, SessionId, TaskId};
use crate::project::Project;
use crate::session::{AgentSession, SessionEnd};
use crate::state::ProjectState;
use crate::task::{Task, TaskStatus};

/// The error a provider can produce. Every trait below reuses this so the core
/// can match on one error vocabulary regardless of which substrate failed.
pub trait Provider: Send + Sync {
    /// The error type this substrate produces.
    type Error: std::error::Error + Send + Sync + 'static;
}

/// A durable piece of project knowledge, as Director sees it.
///
/// This is the boundary view of a memory record — in ai-memory terms, a wiki
/// page; in handoff-mcp terms, a `MemoryEntry`. Director never sees either
/// struct; it sees this. Kept in this module because it exists to be returned
/// by [`MemoryProvider`].
///
/// Note `PartialEq` but not `Eq`: `score` is an `f64`, so total equality is not
/// well defined. Comparisons are for test assertions only.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Memory {
    /// Stable id in the substrate's own namespace, opaque to Director.
    pub id: String,
    /// Short human-facing label.
    pub title: String,
    /// The body, as markdown.
    pub body: String,
    /// Coarse kind, when the substrate reports one.
    pub kind: Option<String>,
    /// Tags / entities the substrate indexed it under.
    pub tags: Vec<String>,
    /// A relevance score the substrate assigned, higher = more relevant. Only
    /// meaningful relative to other results from the same query.
    pub score: Option<f64>,
    /// When the record last changed in its substrate.
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// How to look something up in long-term memory.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MemoryQuery {
    /// Natural-language or keyword search text.
    pub text: String,
    /// Restrict to these tags when the substrate supports it.
    pub tags: Vec<String>,
    /// Maximum results to return.
    pub limit: u32,
}

impl MemoryQuery {
    /// Build a query for `text`, capped at `limit` results.
    pub fn new(text: impl Into<String>, limit: u32) -> Self {
        MemoryQuery {
            text: text.into(),
            tags: vec![],
            limit,
        }
    }
}

/// A command for [`ExecutionProvider`] to run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommandSpec {
    /// The program, e.g. `cargo`.
    pub program: String,
    /// Arguments, in order.
    pub args: Vec<String>,
    /// Working directory to run in.
    pub working_dir: Option<String>,
}

/// What running a command produced. Observed output, never an agent's claim.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommandOutcome {
    /// The process exit code; `-1` if unavailable.
    pub exit_code: i32,
    /// Combined or separated stdout, at the substrate's discretion.
    pub stdout: String,
    /// Standard error, as captured.
    pub stderr: String,
    /// True if the command exceeded its limit and was killed.
    pub timed_out: bool,
}

impl CommandOutcome {
    /// True if the command exited zero without timing out.
    pub fn succeeded(&self) -> bool {
        self.exit_code == 0 && !self.timed_out
    }
}

/// Task storage and dependency handling.
///
/// Maps onto handoff-mcp's task CRUD + `dependencies` + `bulk_update_tasks`.
/// Director is the only writer through this trait in normal operation, which
/// keeps the substrate's file-lock model coherent.
#[async_trait]
pub trait TaskProvider: Provider {
    /// Persist a new task. Returns the task as the substrate stored it, so
    /// callers see any normalization the substrate applied.
    async fn create_task(&self, task: Task) -> Result<Task, Self::Error>;

    /// Fetch a task by Director id. `None` means the substrate has no such task.
    async fn get_task(&self, id: &TaskId) -> Result<Option<Task>, Self::Error>;

    /// Every task the substrate knows about.
    async fn list_tasks(&self) -> Result<Vec<Task>, Self::Error>;

    /// Persist an updated task.
    async fn update_task(&self, task: Task) -> Result<Task, Self::Error>;

    /// Set a task's status. Note that this is a Director-side *recording* of a
    /// decision already made — it is not reachable from an agent's report.
    async fn set_task_status(&self, id: &TaskId, status: TaskStatus) -> Result<(), Self::Error>;

    /// Tasks whose dependencies are all `Done`, ready to be assigned.
    async fn ready_tasks(&self) -> Result<Vec<Task>, Self::Error>;

    /// Record that `task` depends on `dependency`.
    async fn add_dependency(&self, task: &TaskId, dependency: &TaskId) -> Result<(), Self::Error>;
}

/// The agent registry: who is available, and are they still alive.
///
/// Maps onto handoff-mcp's `AgentRecord` with heartbeat TTL/GC.
#[async_trait]
pub trait AgentProvider: Provider {
    /// Register an agent with the substrate.
    async fn register_agent(&self, agent: Agent) -> Result<Agent, Self::Error>;

    /// Look up an agent by id.
    async fn get_agent(&self, id: &AgentId) -> Result<Option<Agent>, Self::Error>;

    /// Every known agent.
    async fn list_agents(&self) -> Result<Vec<Agent>, Self::Error>;

    /// Record a heartbeat for an agent, returning the status the substrate
    /// derives from heartbeat age.
    async fn heartbeat(&self, id: &AgentId) -> Result<AgentStatus, Self::Error>;

    /// Set an agent's status directly (e.g. `Offline` by an operator).
    async fn set_agent_status(&self, id: &AgentId, status: AgentStatus) -> Result<(), Self::Error>;

    /// Agents that could accept work right now.
    async fn available_agents(&self) -> Result<Vec<Agent>, Self::Error>;
}

/// Session lifecycle.
///
/// Maps onto handoff-mcp's session state machine and fork/merge, plus
/// ai-memory's session records.
#[async_trait]
pub trait SessionProvider: Provider {
    /// Record that a session started.
    async fn start_session(&self, session: AgentSession) -> Result<AgentSession, Self::Error>;

    /// Look up a session.
    async fn get_session(&self, id: &SessionId) -> Result<Option<AgentSession>, Self::Error>;

    /// Close a session with a specific end reason.
    async fn close_session(&self, id: &SessionId, end: SessionEnd) -> Result<(), Self::Error>;

    /// Every session that worked on a task, in start order.
    async fn sessions_for_task(&self, task: &TaskId) -> Result<Vec<AgentSession>, Self::Error>;

    /// Fork a session, recording parent lineage.
    async fn fork_session(
        &self,
        parent: &SessionId,
        new_id: SessionId,
    ) -> Result<AgentSession, Self::Error>;
}

/// Director's own claim-once task transfer.
///
/// Not to be confused with ai-memory's `Handoff` row or handoff-mcp's
/// `handoff_notes` — see [`crate::handoff`]. The adapter may store this in
/// Director's own store or model it on a substrate feature; the trait is what
/// the core sees either way.
#[async_trait]
pub trait HandoffProvider: Provider {
    /// Open a handoff addressed to a specific next agent.
    async fn open_handoff(&self, handoff: Handoff) -> Result<Handoff, Self::Error>;

    /// Look up a handoff.
    async fn get_handoff(&self, id: &HandoffId) -> Result<Option<Handoff>, Self::Error>;

    /// Accept a handoff as the intended agent. Must enforce claim-once: only
    /// [`Handoff::to_agent`] may accept, and only while it is still open.
    async fn accept_handoff(
        &self,
        id: &HandoffId,
        acceptor: &AgentId,
        session: &SessionId,
    ) -> Result<Handoff, Self::Error>;

    /// Handoffs still waiting to be claimed.
    async fn open_handoffs(&self) -> Result<Vec<Handoff>, Self::Error>;
}

/// Long-term project knowledge and retrieval.
///
/// Maps onto ai-memory's `memory_query` / `memory_recent` / wiki pages. This is
/// the one place Director deliberately *reuses* a substrate rather than
/// reimplements: retrieval with FTS5, vectors, and decay is already solved
/// well, and rewriting it would be pure loss.
#[async_trait]
pub trait MemoryProvider: Provider {
    /// Save a piece of durable knowledge.
    async fn save_memory(&self, memory: Memory) -> Result<Memory, Self::Error>;

    /// Retrieve knowledge matching a query.
    async fn query_memories(&self, query: &MemoryQuery) -> Result<Vec<Memory>, Self::Error>;

    /// Recently written knowledge, newest first.
    async fn recent_memories(&self, limit: u32) -> Result<Vec<Memory>, Self::Error>;
}

/// Observed project state, straight from git and the filesystem.
///
/// The most rule-laden trait here, and the rule is: **this is observed, never
/// remembered.** A substrate may cache, but the cache is a performance
/// optimization; the truth is what `git` and the filesystem say right now. The
/// verification engine (Phase 10) depends on this being trustworthy.
#[async_trait]
pub trait ProjectStateProvider: Provider {
    /// Observe the current state of a working tree, fresh.
    async fn observe(&self, root: &Path) -> Result<ProjectState, Self::Error>;

    /// The last state observed for a project, if any.
    async fn latest_state(&self, project: &ProjectId) -> Result<Option<ProjectState>, Self::Error>;

    /// Register a project so its state can be tracked.
    async fn register_project(&self, project: Project) -> Result<Project, Self::Error>;
}

/// Running real commands against the project.
///
/// This is what makes Director's verification independent of agent
/// self-report: the verification engine runs `cargo test` through this trait
/// and reads the exit code itself. A substrate that cannot execute commands
/// returns [`UnsupportedCommand`](ProviderError::UnsupportedCommand) and
/// Director falls back to a local executor — but it never falls back to
/// trusting the agent.
#[async_trait]
pub trait ExecutionProvider: Provider {
    /// Run a command and report what happened.
    async fn run_command(&self, command: &CommandSpec) -> Result<CommandOutcome, Self::Error>;
}

/// The standard error variants a provider may report.
///
/// Providers are free to define their own richer error type; this is provided
/// for adapters that have nothing substrate-specific to say.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The substrate has no record of the requested id.
    #[error("not found: {0}")]
    NotFound(String),
    /// The substrate rejected the request because of a conflict (e.g. a claim
    /// lease held by someone else).
    #[error("conflict: {0}")]
    Conflict(String),
    /// The substrate cannot do what was asked (e.g. no command execution).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Talking to the substrate failed.
    #[error("transport: {0}")]
    Transport(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_caps_results() {
        let q = MemoryQuery::new("login auth", 5);
        assert_eq!(q.text, "login auth");
        assert_eq!(q.limit, 5);
        assert!(q.tags.is_empty());
    }

    #[test]
    fn command_success_is_exit_zero_without_timeout() {
        let ok = CommandOutcome {
            exit_code: 0,
            stdout: "1 passed".into(),
            stderr: String::new(),
            timed_out: false,
        };
        let fail = CommandOutcome {
            exit_code: 1,
            stdout: String::new(),
            stderr: "1 failed".into(),
            timed_out: false,
        };
        let hang = CommandOutcome {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
        };

        assert!(ok.succeeded());
        assert!(!fail.succeeded());
        assert!(!hang.succeeded(), "a timed-out command is not a success");
    }

    #[test]
    fn memory_round_trips_through_serde() {
        let m = Memory {
            id: "page-1".into(),
            title: "Auth design".into(),
            body: "we use JWTs".into(),
            kind: Some("decision".into()),
            tags: vec!["auth".into()],
            score: Some(0.9),
            updated_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: Memory = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    /// The provider error vocabulary formats usefully, so the loop's diagnostics
    /// stay readable regardless of which substrate failed.
    #[test]
    fn provider_errors_have_readable_messages() {
        let e = ProviderError::NotFound("TASK-42".into());
        assert!(format!("{e}").contains("TASK-42"));
        assert!(format!("{}", ProviderError::Conflict("lease held".into())).contains("conflict"));
        assert!(format!("{}", ProviderError::Unsupported("x".into())).contains("unsupported"));
        assert!(
            format!("{}", ProviderError::Transport("connection reset".into()))
                .contains("transport")
        );
    }
}
