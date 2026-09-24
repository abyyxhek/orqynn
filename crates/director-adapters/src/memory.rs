//! [`InMemoryProvider`] — a complete in-process substrate.
//!
//! One struct, every provider trait, backed by `HashMap`s. It is deliberately
//! **faithful to the trait contracts, not to any real substrate**: it enforces
//! the same invariants a real adapter must (claim-once handoffs, unique ids,
//! dependency-aware readiness) so that a test passing against it is meaningful
//! evidence about the loop, not a tautology.
//!
//! ## Seeding
//!
//! Tests need to arrange project state without a git repository, so
//! [`seed_state`](InMemoryProvider::seed_state) lets a caller say "if anyone
//! observes this path, here is what they will see". [`observe`](InMemoryProvider::observe)
//! returns the seeded state or an error — it never invents one, because
//! inventing observed state is exactly the failure mode the trait exists to
//! prevent.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;

use director_domain::agent::{Agent, AgentStatus};
use director_domain::handoff::{Handoff, HandoffError, HandoffState};
use director_domain::ids::{AgentId, HandoffId, ProjectId, SessionId, TaskId};
use director_domain::project::Project;
use director_domain::providers::{
    AgentProvider, HandoffProvider, MemoryProvider, MemoryQuery, ProjectStateProvider, Provider,
    ProviderError, SessionProvider, TaskProvider,
};
use director_domain::session::{AgentSession, SessionEnd};
use director_domain::state::ProjectState;
use director_domain::task::{Task, TaskStatus};

/// Errors the in-memory substrate can produce.
#[derive(Debug, thiserror::Error)]
pub enum InMemoryError {
    /// No record with that id exists.
    #[error("not found: {0}")]
    NotFound(String),
    /// A record with that id already exists.
    #[error("already exists: {0}")]
    AlreadyExists(String),
    /// A handoff was consumed or addressed elsewhere.
    #[error("handoff error: {0}")]
    Handoff(#[from] HandoffError),
}

/// Internal state, behind a `Mutex`. Methods never hold the lock across an
/// await point — there are no await points inside a critical section.
#[derive(Debug, Default)]
struct State {
    tasks: HashMap<TaskId, Task>,
    agents: HashMap<AgentId, Agent>,
    sessions: HashMap<SessionId, AgentSession>,
    handoffs: HashMap<HandoffId, Handoff>,
    memories: Vec<director_domain::providers::Memory>,
    projects: HashMap<ProjectId, Project>,
    /// Seeded observations, keyed by working-tree path.
    states: HashMap<PathBuf, ProjectState>,
}

/// An in-process implementation of every provider trait.
///
/// Cloneable so tests can hand a copy to the code under test while keeping a
/// handle to make assertions. All clones share one underlying state.
#[derive(Debug, Clone, Default)]
pub struct InMemoryProvider {
    state: std::sync::Arc<Mutex<State>>,
}

impl InMemoryProvider {
    /// Create an empty provider.
    pub fn new() -> Self {
        InMemoryProvider::default()
    }

    /// Seed the observed state for a working tree, so `observe` has something
    /// to return without a real repository.
    pub fn seed_state(&self, root: &Path, state: ProjectState) {
        let mut guard = self.state.lock().expect("in-memory state poisoned");
        guard.states.insert(root.to_path_buf(), state);
    }

    /// Number of tasks stored. Useful for assertions.
    pub fn task_count(&self) -> usize {
        self.state.lock().expect("state poisoned").tasks.len()
    }

    /// Number of agents stored.
    pub fn agent_count(&self) -> usize {
        self.state.lock().expect("state poisoned").agents.len()
    }
}

impl Provider for InMemoryProvider {
    type Error = InMemoryError;
}

#[async_trait]
impl TaskProvider for InMemoryProvider {
    async fn create_task(&self, task: Task) -> Result<Task, Self::Error> {
        let mut guard = self.state.lock().expect("state poisoned");
        if guard.tasks.contains_key(&task.id) {
            return Err(InMemoryError::AlreadyExists(task.id.to_string()));
        }
        let stored = task.clone();
        guard.tasks.insert(task.id.clone(), stored);
        Ok(task)
    }

    async fn get_task(&self, id: &TaskId) -> Result<Option<Task>, Self::Error> {
        let guard = self.state.lock().expect("state poisoned");
        Ok(guard.tasks.get(id).cloned())
    }

    async fn list_tasks(&self) -> Result<Vec<Task>, Self::Error> {
        let guard = self.state.lock().expect("state poisoned");
        let mut tasks: Vec<Task> = guard.tasks.values().cloned().collect();
        // Deterministic order: tests must not depend on HashMap iteration.
        tasks.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        Ok(tasks)
    }

    async fn update_task(&self, task: Task) -> Result<Task, Self::Error> {
        let mut guard = self.state.lock().expect("state poisoned");
        if !guard.tasks.contains_key(&task.id) {
            return Err(InMemoryError::NotFound(task.id.to_string()));
        }
        let stored = task.clone();
        guard.tasks.insert(task.id.clone(), stored);
        Ok(task)
    }

    async fn set_task_status(&self, id: &TaskId, status: TaskStatus) -> Result<(), Self::Error> {
        let mut guard = self.state.lock().expect("state poisoned");
        let task = guard
            .tasks
            .get_mut(id)
            .ok_or_else(|| InMemoryError::NotFound(id.to_string()))?;
        task.status = status;
        task.touch();
        Ok(())
    }

    async fn ready_tasks(&self) -> Result<Vec<Task>, Self::Error> {
        // Pure function over current statuses, mirroring Task::is_ready_given:
        // Director never trusts a stored "ready" flag.
        let guard = self.state.lock().expect("state poisoned");
        let statuses: HashMap<TaskId, TaskStatus> = guard
            .tasks
            .iter()
            .map(|(id, t)| (id.clone(), t.status))
            .collect();

        let mut ready: Vec<Task> = guard
            .tasks
            .values()
            .filter(|t| t.is_ready_given(|dep| statuses.get(dep).copied()))
            .filter(|t| !t.status.is_terminal())
            .cloned()
            .collect();
        ready.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        Ok(ready)
    }

    async fn add_dependency(&self, task: &TaskId, dependency: &TaskId) -> Result<(), Self::Error> {
        let mut guard = self.state.lock().expect("state poisoned");
        let owner = guard
            .tasks
            .get_mut(task)
            .ok_or_else(|| InMemoryError::NotFound(task.to_string()))?;
        if !owner.dependencies.contains(dependency) {
            owner.dependencies.push(dependency.clone());
            owner.touch();
        }
        Ok(())
    }
}

#[async_trait]
impl AgentProvider for InMemoryProvider {
    async fn register_agent(&self, agent: Agent) -> Result<Agent, Self::Error> {
        let mut guard = self.state.lock().expect("state poisoned");
        if guard.agents.contains_key(&agent.id) {
            return Err(InMemoryError::AlreadyExists(agent.id.to_string()));
        }
        let stored = agent.clone();
        guard.agents.insert(agent.id.clone(), stored);
        Ok(agent)
    }

    async fn get_agent(&self, id: &AgentId) -> Result<Option<Agent>, Self::Error> {
        let guard = self.state.lock().expect("state poisoned");
        Ok(guard.agents.get(id).cloned())
    }

    async fn list_agents(&self) -> Result<Vec<Agent>, Self::Error> {
        let guard = self.state.lock().expect("state poisoned");
        let mut agents: Vec<Agent> = guard.agents.values().cloned().collect();
        agents.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        Ok(agents)
    }

    async fn heartbeat(&self, id: &AgentId) -> Result<AgentStatus, Self::Error> {
        let mut guard = self.state.lock().expect("state poisoned");
        let agent = guard
            .agents
            .get_mut(id)
            .ok_or_else(|| InMemoryError::NotFound(id.to_string()))?;
        agent.heartbeat();
        // Derived, never stored: liveness is a function of "how long since we
        // heard from this agent", computed when asked. The recorded `status`
        // is left untouched — availability is a function of assignment, which
        // is the assignment service's decision, not the substrate's.
        let age = chrono::Utc::now() - agent.last_seen;
        Ok(Agent::status_from_heartbeat(age))
    }

    async fn set_agent_status(&self, id: &AgentId, status: AgentStatus) -> Result<(), Self::Error> {
        let mut guard = self.state.lock().expect("state poisoned");
        let agent = guard
            .agents
            .get_mut(id)
            .ok_or_else(|| InMemoryError::NotFound(id.to_string()))?;
        agent.status = status;
        Ok(())
    }

    async fn available_agents(&self) -> Result<Vec<Agent>, Self::Error> {
        let guard = self.state.lock().expect("state poisoned");
        let mut agents: Vec<Agent> = guard
            .agents
            .values()
            .filter(|a| a.status.can_accept_work())
            .cloned()
            .collect();
        agents.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        Ok(agents)
    }
}

#[async_trait]
impl SessionProvider for InMemoryProvider {
    async fn start_session(&self, session: AgentSession) -> Result<AgentSession, Self::Error> {
        let mut guard = self.state.lock().expect("state poisoned");
        if guard.sessions.contains_key(&session.id) {
            return Err(InMemoryError::AlreadyExists(session.id.to_string()));
        }
        let stored = session.clone();
        guard.sessions.insert(session.id.clone(), stored);
        Ok(session)
    }

    async fn get_session(&self, id: &SessionId) -> Result<Option<AgentSession>, Self::Error> {
        let guard = self.state.lock().expect("state poisoned");
        Ok(guard.sessions.get(id).cloned())
    }

    async fn close_session(&self, id: &SessionId, end: SessionEnd) -> Result<(), Self::Error> {
        let mut guard = self.state.lock().expect("state poisoned");
        let session = guard
            .sessions
            .get_mut(id)
            .ok_or_else(|| InMemoryError::NotFound(id.to_string()))?;
        session.close(end);
        Ok(())
    }

    async fn sessions_for_task(&self, task: &TaskId) -> Result<Vec<AgentSession>, Self::Error> {
        let guard = self.state.lock().expect("state poisoned");
        let mut sessions: Vec<AgentSession> = guard
            .sessions
            .values()
            .filter(|s| s.task_id.as_ref() == Some(task))
            .cloned()
            .collect();
        sessions.sort_by_key(|s| s.started_at);
        Ok(sessions)
    }

    async fn fork_session(
        &self,
        parent: &SessionId,
        new_id: SessionId,
    ) -> Result<AgentSession, Self::Error> {
        let mut guard = self.state.lock().expect("state poisoned");
        let existing = guard
            .sessions
            .get(parent)
            .ok_or_else(|| InMemoryError::NotFound(parent.to_string()))?;
        if guard.sessions.contains_key(&new_id) {
            return Err(InMemoryError::AlreadyExists(new_id.to_string()));
        }
        let child = existing.fork(new_id);
        guard.sessions.insert(child.id.clone(), child.clone());
        Ok(child)
    }
}

#[async_trait]
impl HandoffProvider for InMemoryProvider {
    async fn open_handoff(&self, handoff: Handoff) -> Result<Handoff, Self::Error> {
        let mut guard = self.state.lock().expect("state poisoned");
        if guard.handoffs.contains_key(&handoff.id) {
            return Err(InMemoryError::AlreadyExists(handoff.id.to_string()));
        }
        let stored = handoff.clone();
        guard.handoffs.insert(handoff.id.clone(), stored);
        Ok(handoff)
    }

    async fn get_handoff(&self, id: &HandoffId) -> Result<Option<Handoff>, Self::Error> {
        let guard = self.state.lock().expect("state poisoned");
        Ok(guard.handoffs.get(id).cloned())
    }

    async fn accept_handoff(
        &self,
        id: &HandoffId,
        acceptor: &AgentId,
        session: &SessionId,
    ) -> Result<Handoff, Self::Error> {
        let mut guard = self.state.lock().expect("state poisoned");
        let handoff = guard
            .handoffs
            .get_mut(id)
            .ok_or_else(|| InMemoryError::NotFound(id.to_string()))?;
        // The claim-once guard lives on the entity, so every adapter gets it
        // for free rather than having to remember to reimplement it.
        handoff.accept(acceptor.clone(), session.clone())?;
        Ok(handoff.clone())
    }

    async fn open_handoffs(&self) -> Result<Vec<Handoff>, Self::Error> {
        let guard = self.state.lock().expect("state poisoned");
        let mut handoffs: Vec<Handoff> = guard
            .handoffs
            .values()
            .filter(|h| h.state == HandoffState::Open)
            .cloned()
            .collect();
        handoffs.sort_by_key(|h| h.created_at);
        Ok(handoffs)
    }
}

#[async_trait]
impl MemoryProvider for InMemoryProvider {
    async fn save_memory(
        &self,
        memory: director_domain::providers::Memory,
    ) -> Result<director_domain::providers::Memory, Self::Error> {
        let mut guard = self.state.lock().expect("state poisoned");
        guard.memories.push(memory.clone());
        Ok(memory)
    }

    async fn query_memories(
        &self,
        query: &MemoryQuery,
    ) -> Result<Vec<director_domain::providers::Memory>, Self::Error> {
        let guard = self.state.lock().expect("state poisoned");
        let needle = query.text.to_lowercase();

        let mut hits: Vec<_> = guard
            .memories
            .iter()
            .filter(|m| {
                // Deliberately naive lexical match: this stands in for
                // ai-memory's FTS5+vector RRF. Real retrieval is a substrate
                // responsibility, not Director's.
                let body = m.body.to_lowercase();
                let title = m.title.to_lowercase();
                needle
                    .split_whitespace()
                    .all(|word| body.contains(word) || title.contains(word))
            })
            .take(query.limit.max(1) as usize)
            .cloned()
            .collect();
        hits.sort_by_key(|m| std::cmp::Reverse(m.updated_at));
        Ok(hits)
    }

    async fn recent_memories(
        &self,
        limit: u32,
    ) -> Result<Vec<director_domain::providers::Memory>, Self::Error> {
        let guard = self.state.lock().expect("state poisoned");
        let mut memories = guard.memories.clone();
        memories.sort_by_key(|m| std::cmp::Reverse(m.updated_at));
        memories.truncate(limit.max(1) as usize);
        Ok(memories)
    }
}

#[async_trait]
impl ProjectStateProvider for InMemoryProvider {
    async fn observe(&self, root: &Path) -> Result<ProjectState, Self::Error> {
        // Never invents state: if nobody seeded this path, there is nothing to
        // observe, and pretending otherwise is the exact failure mode the
        // trait exists to prevent.
        let guard = self.state.lock().expect("state poisoned");
        guard
            .states
            .get(root)
            .cloned()
            .ok_or_else(|| InMemoryError::NotFound(root.display().to_string()))
    }

    async fn latest_state(&self, project: &ProjectId) -> Result<Option<ProjectState>, Self::Error> {
        let guard = self.state.lock().expect("state poisoned");
        Ok(guard
            .projects
            .get(project)
            .and_then(|p| guard.states.get(Path::new(&p.root)))
            .cloned())
    }

    async fn register_project(&self, project: Project) -> Result<Project, Self::Error> {
        let mut guard = self.state.lock().expect("state poisoned");
        if guard.projects.contains_key(&project.id) {
            return Err(InMemoryError::AlreadyExists(project.id.to_string()));
        }
        let stored = project.clone();
        guard.projects.insert(project.id.clone(), stored);
        Ok(project)
    }
}

/// Convert an in-memory error into the generic provider vocabulary, so callers
/// that do not care which substrate failed can match on one type.
impl From<InMemoryError> for ProviderError {
    fn from(err: InMemoryError) -> Self {
        match err {
            InMemoryError::NotFound(what) => ProviderError::NotFound(what),
            InMemoryError::AlreadyExists(what) => ProviderError::Conflict(what),
            InMemoryError::Handoff(HandoffError::WrongAgent) => {
                ProviderError::Conflict("handoff addressed to a different agent".into())
            }
            InMemoryError::Handoff(HandoffError::AlreadyConsumed) => {
                ProviderError::Conflict("handoff already consumed".into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use director_domain::agent::{Agent, Harness};
    use director_domain::handoff::Handoff;
    use director_domain::ids::{MachineId, TaskId};
    use director_domain::providers::{Memory, MemoryQuery};
    use director_domain::state::ProjectState;
    use director_domain::task::{Task, TaskStatus};

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn task(id: &str) -> Task {
        Task::new(TaskId::from_string(id), id, "objective")
    }

    #[test]
    fn a_task_can_be_created_and_fetched() {
        let p = InMemoryProvider::new();
        let rt = rt();
        let created = rt.block_on(p.create_task(task("TASK-1"))).unwrap();
        let fetched = rt
            .block_on(p.get_task(&TaskId::from_string("TASK-1")))
            .unwrap();
        assert_eq!(created, fetched.unwrap());
        assert_eq!(p.task_count(), 1);
    }

    #[test]
    fn duplicate_task_creation_is_rejected() {
        let p = InMemoryProvider::new();
        let rt = rt();
        rt.block_on(p.create_task(task("TASK-1"))).unwrap();
        let err = rt.block_on(p.create_task(task("TASK-1"))).unwrap_err();
        assert!(matches!(err, InMemoryError::AlreadyExists(_)));
    }

    #[test]
    fn a_missing_task_is_none_not_an_error() {
        let p = InMemoryProvider::new();
        let rt = rt();
        assert!(rt
            .block_on(p.get_task(&TaskId::from_string("nope")))
            .unwrap()
            .is_none());
    }

    #[test]
    fn status_changes_are_persisted() {
        let p = InMemoryProvider::new();
        let rt = rt();
        rt.block_on(p.create_task(task("TASK-1"))).unwrap();
        rt.block_on(p.set_task_status(&TaskId::from_string("TASK-1"), TaskStatus::InProgress))
            .unwrap();
        let t = rt
            .block_on(p.get_task(&TaskId::from_string("TASK-1")))
            .unwrap()
            .unwrap();
        assert_eq!(t.status, TaskStatus::InProgress);
    }

    #[test]
    fn readiness_follows_dependencies() {
        let p = InMemoryProvider::new();
        let rt = rt();
        rt.block_on(p.create_task(task("DB-1"))).unwrap();
        rt.block_on(p.create_task(task("AUTH-1"))).unwrap();
        rt.block_on(p.add_dependency(&TaskId::from_string("AUTH-1"), &TaskId::from_string("DB-1")))
            .unwrap();

        // DB-1 has no dependencies and is not terminal, so it is ready.
        // AUTH-1 is not: its dependency is still outstanding.
        let ready: Vec<_> = rt
            .block_on(p.ready_tasks())
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ready, vec![TaskId::from_string("DB-1")]);

        rt.block_on(p.set_task_status(&TaskId::from_string("DB-1"), TaskStatus::Done))
            .unwrap();
        let ready: Vec<_> = rt
            .block_on(p.ready_tasks())
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ready, vec![TaskId::from_string("AUTH-1")]);
    }

    #[test]
    fn a_completed_task_is_not_ready_again() {
        let p = InMemoryProvider::new();
        let rt = rt();
        rt.block_on(p.create_task(task("AUTH-1"))).unwrap();
        rt.block_on(p.set_task_status(&TaskId::from_string("AUTH-1"), TaskStatus::Done))
            .unwrap();
        assert!(rt.block_on(p.ready_tasks()).unwrap().is_empty());
    }

    #[test]
    fn an_agent_registers_and_heartbeats() {
        let p = InMemoryProvider::new();
        let rt = rt();
        let agent = Agent::register(
            AgentId::from_string("AGENT-1"),
            "claude",
            Harness::ClaudeCode,
            MachineId::from_string("MACH-a"),
            vec![],
        );
        rt.block_on(p.register_agent(agent)).unwrap();

        // Freshly registered and unclaimed: available for work.
        assert_eq!(rt.block_on(p.available_agents()).unwrap().len(), 1);

        // A heartbeat reports liveness *derived from age*. It deliberately does
        // not overwrite the recorded status: availability is a function of
        // assignment, which is the Phase 8 service's call, not the substrate's.
        let status = rt
            .block_on(p.heartbeat(&AgentId::from_string("AGENT-1")))
            .unwrap();
        assert_eq!(status, AgentStatus::Busy);
        let recorded = rt
            .block_on(p.get_agent(&AgentId::from_string("AGENT-1")))
            .unwrap()
            .unwrap();
        assert_eq!(recorded.status, AgentStatus::Available);

        // Availability changes only through an explicit status change.
        rt.block_on(p.set_agent_status(&AgentId::from_string("AGENT-1"), AgentStatus::Offline))
            .unwrap();
        assert!(rt.block_on(p.available_agents()).unwrap().is_empty());
    }

    #[test]
    fn sessions_accumulate_per_task() {
        let p = InMemoryProvider::new();
        let rt = rt();
        let s1 = AgentSession::start(
            SessionId::from_string("SESS-1"),
            AgentId::from_string("AGENT-1"),
            MachineId::from_string("MACH-a"),
            Some(TaskId::from_string("AUTH-1")),
        );
        rt.block_on(p.start_session(s1)).unwrap();
        let child = rt
            .block_on(p.fork_session(
                &SessionId::from_string("SESS-1"),
                SessionId::from_string("SESS-2"),
            ))
            .unwrap();
        assert_eq!(
            child.parent_session_id,
            Some(SessionId::from_string("SESS-1"))
        );

        let for_task = rt
            .block_on(p.sessions_for_task(&TaskId::from_string("AUTH-1")))
            .unwrap();
        assert_eq!(for_task.len(), 2);

        rt.block_on(p.close_session(&SessionId::from_string("SESS-1"), SessionEnd::Clean))
            .unwrap();
        assert_eq!(
            rt.block_on(p.get_session(&SessionId::from_string("SESS-1")))
                .unwrap()
                .unwrap()
                .status,
            director_domain::session::SessionStatus::Closed
        );
    }

    #[test]
    fn handoffs_are_claim_once() {
        let p = InMemoryProvider::new();
        let rt = rt();
        let h = Handoff::open(
            HandoffId::from_string("HOF-1"),
            TaskId::from_string("AUTH-1"),
            AgentId::from_string("AGENT-claude"),
            AgentId::from_string("AGENT-codex"),
            "summary",
        );
        rt.block_on(p.open_handoff(h)).unwrap();
        assert_eq!(rt.block_on(p.open_handoffs()).unwrap().len(), 1);

        // Wrong agent cannot accept.
        assert!(rt
            .block_on(p.accept_handoff(
                &HandoffId::from_string("HOF-1"),
                &AgentId::from_string("AGENT-deepseek"),
                &SessionId::from_string("SESS-3")
            ))
            .is_err());
        assert_eq!(rt.block_on(p.open_handoffs()).unwrap().len(), 1);

        // Right agent can.
        rt.block_on(p.accept_handoff(
            &HandoffId::from_string("HOF-1"),
            &AgentId::from_string("AGENT-codex"),
            &SessionId::from_string("SESS-2"),
        ))
        .unwrap();
        assert!(rt.block_on(p.open_handoffs()).unwrap().is_empty());

        // And only once.
        assert!(rt
            .block_on(p.accept_handoff(
                &HandoffId::from_string("HOF-1"),
                &AgentId::from_string("AGENT-codex"),
                &SessionId::from_string("SESS-2")
            ))
            .is_err());
    }

    #[test]
    fn memories_are_queryable() {
        let p = InMemoryProvider::new();
        let rt = rt();
        let now = chrono::Utc::now();
        for (title, body) in [
            ("Auth design", "we chose stateless JWTs for login"),
            ("DB schema", "the users table holds credentials"),
        ] {
            rt.block_on(p.save_memory(Memory {
                id: title.to_string(),
                title: title.into(),
                body: body.into(),
                kind: Some("decision".into()),
                tags: vec![],
                score: None,
                updated_at: now,
            }))
            .unwrap();
        }

        let hits = rt
            .block_on(p.query_memories(&MemoryQuery::new("JWTs login", 10)))
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Auth design");

        let recent = rt.block_on(p.recent_memories(5)).unwrap();
        assert_eq!(recent.len(), 2);
    }

    #[test]
    fn observing_an_unseeded_path_is_an_error_not_invention() {
        let p = InMemoryProvider::new();
        let rt = rt();
        let err = rt.block_on(p.observe(Path::new("/nope"))).unwrap_err();
        assert!(matches!(err, InMemoryError::NotFound(_)));
    }

    #[test]
    fn seeded_state_is_returned_verbatim() {
        let p = InMemoryProvider::new();
        let rt = rt();
        let state = ProjectState {
            branch: Some("main".into()),
            head_commit: "abc".into(),
            working_tree: vec![],
            recent_commits: vec![],
            test_results: None,
            observed_on: MachineId::from_string("MACH-a"),
            observed_at: chrono::Utc::now(),
        };
        p.seed_state(Path::new("/repo"), state.clone());
        let observed = rt.block_on(p.observe(Path::new("/repo"))).unwrap();
        assert_eq!(observed, state);
    }

    #[test]
    fn clones_share_state() {
        let p = InMemoryProvider::new();
        let clone = p.clone();
        let rt = rt();
        rt.block_on(clone.create_task(task("TASK-1"))).unwrap();
        assert_eq!(p.task_count(), 1);
    }
}
