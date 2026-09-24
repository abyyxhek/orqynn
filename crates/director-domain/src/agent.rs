//! [Agent] and [Machine]: the replaceable side of the system.
//!
//! Everything here is designed around one fact: **agents disappear**. A
//! session ends, a cloud token expires, a laptop closes, a harness is
//! uninstalled. Director models agents as ephemeral and heartbeated, never as
//! owners of work.

use serde::{Deserialize, Serialize};

use crate::capability::Capability;
use crate::ids::{AgentId, MachineId, TaskId};

/// Which harness an agent runs in. Director is vendor-independent: this enum
/// carries no behavior, it is metadata for logging, capability defaults, and
/// human-facing display.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    /// Anthropic's Claude Code CLI.
    ClaudeCode,
    /// OpenAI's Codex CLI.
    Codex,
    /// DeepSeek's coding agent.
    DeepSeek,
    /// The OpenCode CLI.
    OpenCode,
    /// Block's Goose agent.
    Goose,
    /// The Cursor editor's built-in agent.
    Cursor,
    /// Google's Gemini CLI.
    GeminiCli,
    /// Google's Antigravity CLI.
    Antigravity,
    /// Infinite's Crush agent.
    Crush,
    /// Moonshot's Kimi Code.
    KimiCode,
    /// A human working directly in the repository.
    Human,
    /// Any MCP/ACP-capable agent Director has not been taught about. Kept so
    /// that adding a harness never requires changing Director's model.
    Other(String),
}

impl Harness {
    /// Free-form harness name for an unknown agent.
    pub fn other(name: impl Into<String>) -> Self {
        Harness::Other(name.into())
    }
}

impl std::fmt::Display for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Harness::Other(name) => f.write_str(name),
            h => f.write_str(&serde_json::to_string(h).unwrap_or_else(|_| "\"?\"".into())),
        }
    }
}

/// Lifecycle state of an agent, derived from heartbeats (Phase 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Recently heartbeaten and not currently claimed to a task.
    Available,
    /// Recently heartbeaten and working on a task.
    Busy,
    /// Heartbeat is stale: the agent has gone quiet but has not been gone long
    /// enough to be declared dead.
    Stale,
    /// Past the stale window with no heartbeat. Its lease may be reclaimed.
    Disconnected,
    /// Explicitly deregistered or marked unavailable by an operator.
    Offline,
}

impl AgentStatus {
    /// True if the agent could be given work right now.
    pub fn can_accept_work(self) -> bool {
        matches!(self, AgentStatus::Available)
    }

    /// True if Director should treat the agent as possibly still alive.
    pub fn is_live(self) -> bool {
        matches!(
            self,
            AgentStatus::Available | AgentStatus::Busy | AgentStatus::Stale
        )
    }

    /// True if the agent has gone away and its claims are reclaimable.
    pub fn is_gone(self) -> bool {
        matches!(self, AgentStatus::Disconnected | AgentStatus::Offline)
    }
}

/// A worker: one harness running as one process on one machine.
///
/// Deliberately thin. The rich state — what it is working on — lives in
/// [crate::assignment::AgentAssignment], because that state must survive this
/// agent's disappearance and be handed to its replacement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    /// This agent's stable identifier.
    pub id: AgentId,
    /// Display name for logs and UI.
    pub name: String,
    /// Which harness this agent runs in.
    pub harness: Harness,
    /// Model or tier, free-form, e.g. "opus" or "deepseek-v3". Director never
    /// branches on this; it is recorded for attribution.
    pub model: Option<String>,
    /// What this agent declares it can do.
    pub capabilities: Vec<Capability>,
    /// The machine it runs on.
    pub machine: MachineId,
    /// Derived liveness state, from heartbeats.
    pub status: AgentStatus,
    /// Task this agent is currently assigned. Authoritative source is the
    /// assignment record; this is a denormalized view for fast lookups and is
    /// reconciled by the assignment service.
    pub current_task: Option<TaskId>,
    /// When Director first registered the agent.
    pub registered_at: chrono::DateTime<chrono::Utc>,
    /// When Director last heard from the agent.
    pub last_seen: chrono::DateTime<chrono::Utc>,
}

impl Agent {
    /// Register a new agent as `Available` with no current task.
    pub fn register(
        id: AgentId,
        name: impl Into<String>,
        harness: Harness,
        machine: MachineId,
        capabilities: Vec<Capability>,
    ) -> Self {
        let now = chrono::Utc::now();
        Agent {
            id,
            name: name.into(),
            harness,
            model: None,
            capabilities,
            machine,
            status: AgentStatus::Available,
            current_task: None,
            registered_at: now,
            last_seen: now,
        }
    }

    /// True if this agent declares a capability satisfying `required`.
    pub fn has_capability(&self, required: &Capability) -> bool {
        self.capabilities.iter().any(|c| c.matches(required))
    }

    /// True if the agent declares every required capability.
    pub fn has_all_capabilities(&self, required: &[Capability]) -> bool {
        required.iter().all(|r| self.has_capability(r))
    }

    /// Record a heartbeat, returning the previous `last_seen`.
    pub fn heartbeat(&mut self) -> chrono::DateTime<chrono::Utc> {
        let previous = self.last_seen;
        self.last_seen = chrono::Utc::now();
        previous
    }

    /// Derive status from heartbeat age.
    ///
    /// Mirrors handoff-mcp's `AgentRecord` TTL design (30 min to `Stale`,
    /// twice that to `Disconnected`) so Director's notion of liveness agrees
    /// with the substrate's when both are running.
    pub fn status_from_heartbeat(age: chrono::Duration) -> AgentStatus {
        let mins = age.num_minutes();
        if mins < 30 {
            AgentStatus::Busy
        } else if mins < 60 {
            AgentStatus::Stale
        } else {
            AgentStatus::Disconnected
        }
    }
}

/// A physical or virtual machine an agent runs on.
///
/// Machines exist because the original problem is often *the machine* going
/// away, not the agent: the laptop closes, the cloud session expires. Director
/// must be able to say "the work was on MACHINE-A; resume it on MACHINE-B".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    /// This machine's identifier.
    pub id: MachineId,
    /// The machine's network hostname.
    pub hostname: String,
    /// Free-form label, e.g. "Lin's MacBook" or "ci-runner-4".
    pub label: Option<String>,
    /// Last time an agent on this machine heartbeaten.
    pub last_seen: chrono::DateTime<chrono::Utc>,
    /// Whether an agent on it has heartbeaten recently.
    pub online: bool,
}

impl Machine {
    /// Create a newly-online machine record.
    pub fn new(id: MachineId, hostname: impl Into<String>) -> Self {
        Machine {
            id,
            hostname: hostname.into(),
            label: None,
            last_seen: chrono::Utc::now(),
            online: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(caps: Vec<Capability>) -> Agent {
        Agent::register(
            AgentId::from_string("AGENT-1"),
            "claude-a",
            Harness::ClaudeCode,
            MachineId::from_string("MACH-a"),
            caps,
        )
    }

    #[test]
    fn a_registered_agent_is_available_with_no_task() {
        let a = agent(vec![Capability::Coding]);
        assert_eq!(a.status, AgentStatus::Available);
        assert!(a.current_task.is_none());
        assert!(a.status.can_accept_work());
    }

    #[test]
    fn capability_matching_is_used_for_assignment_eligibility() {
        let generalist = agent(vec![Capability::Coding]);
        let specialist = agent(vec![Capability::Database]);

        assert!(generalist.has_all_capabilities(&[Capability::Database]));
        assert!(specialist.has_all_capabilities(&[Capability::Database]));
        assert!(!specialist.has_all_capabilities(&[Capability::Frontend]));
    }

    #[test]
    fn heartbeat_advances_last_seen() {
        let mut a = agent(vec![]);
        let before = a.last_seen;
        std::thread::sleep(std::time::Duration::from_millis(2));
        let previous = a.heartbeat();
        assert_eq!(previous, before);
        assert!(a.last_seen > before);
    }

    #[test]
    fn liveness_windows_follow_the_substrate_convention() {
        assert_eq!(
            Agent::status_from_heartbeat(chrono::Duration::minutes(5)),
            AgentStatus::Busy
        );
        assert_eq!(
            Agent::status_from_heartbeat(chrono::Duration::minutes(45)),
            AgentStatus::Stale
        );
        assert_eq!(
            Agent::status_from_heartbeat(chrono::Duration::minutes(90)),
            AgentStatus::Disconnected
        );
    }

    #[test]
    fn gone_agents_cannot_accept_work() {
        assert!(AgentStatus::Available.can_accept_work());
        assert!(!AgentStatus::Busy.can_accept_work());
        assert!(!AgentStatus::Stale.can_accept_work());
        assert!(!AgentStatus::Disconnected.can_accept_work());
        assert!(!AgentStatus::Offline.can_accept_work());

        assert!(AgentStatus::Stale.is_live());
        assert!(AgentStatus::Disconnected.is_gone());
        assert!(AgentStatus::Offline.is_gone());
    }

    #[test]
    fn unknown_harnesses_round_trip_without_data_loss() {
        let h = Harness::other("kimi-cli");
        let json = serde_json::to_string(&h).unwrap();
        let back: Harness = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn agent_round_trips_through_serde() {
        let a = agent(vec![Capability::Coding, Capability::Testing]);
        let json = serde_json::to_string(&a).unwrap();
        let back: Agent = serde_json::from_str(&json).unwrap();
        assert_eq!(a, back);
    }
}
