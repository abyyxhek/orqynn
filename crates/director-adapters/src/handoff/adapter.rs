//! [`HandoffAdapter`] — Director's provider traits over a live handoff-mcp.
//!
//! This is the last piece of Phase 2's handoff half: the struct that makes
//! [`TaskProvider`], [`AgentProvider`], and [`SessionProvider`] real by
//! composing the two layers underneath it — [`transport`] to reach the
//! substrate, and [`mapping`] to translate what comes back.
//!
//! ## What the adapter adds on top of the two layers
//!
//! The mapping is stateless and the transport is a pipe; neither can carry
//! Director's own knowledge across calls. Three things live here for that
//! reason:
//!
//! 1. **The trusted-done set.** Director's verification rule needs to know
//!    which `done` states Director itself produced. The substrate cannot tell
//!    us (see [`mapping`]'s note on the closed `extra` channel), so the adapter
//!    keeps the set itself and hands it to [`mapping::task_from_wire`] on every
//!    read. It is populated only by Director's own writes, so an agent cannot
//!    put an id into it.
//! 2. **Write-path repairs.** Two substrate rules would otherwise reject
//!    Director's writes: a `done` transition requires every criterion checked,
//!    and any `in_progress`/`done` write requires an estimate. The adapter
//!    ticks criteria it has verified and disables the estimate requirement at
//!    project setup, rather than fabricating hours.
//! 3. **Read-back after write.** `handoff_update_task` answers with prose
//!    (`"Created task t1: Title [todo]"`), so the adapter re-reads the task to
//!    return the record the substrate actually stored.
//!
//! ## The test seam
//!
//! Every method goes through [`HandoffWire`], a one-method trait that the real
//! [`McpTransport`] satisfies and a fake can satisfy too. That is why the
//! adapter's logic — including the verification rule and the write repairs — is
//! unit-testable with no child process, and why the tests in this file are
//! evidence about the adapter rather than about a spawn succeeding.
//!
//! ## What the adapter deliberately does not implement
//!
//! [`HandoffProvider`], [`MemoryProvider`], [`ProjectStateProvider`], and
//! [`ExecutionProvider`] are not implemented here. handoff-mcp's handoff notion
//! is session-scoped notes, not Director's claim-once transfer; its memory
//! tools belong to the ai-memory adapter (Phase 3); and observing git is the
//! git module's job, not a substrate's. Where a trait method has no honest
//! substrate equivalent at all, the adapter returns
//! [`HandoffAdapterError::Unsupported`] rather than approximating silently.
//!
//! [`TaskProvider`]: director_domain::providers::TaskProvider
//! [`AgentProvider`]: director_domain::providers::AgentProvider
//! [`SessionProvider`]: director_domain::providers::SessionProvider
//! [`HandoffProvider`]: director_domain::providers::HandoffProvider
//! [`MemoryProvider`]: director_domain::providers::MemoryProvider
//! [`ProjectStateProvider`]: director_domain::providers::ProjectStateProvider
//! [`ExecutionProvider`]: director_domain::providers::ExecutionProvider

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use director_domain::agent::{Agent, AgentStatus};
use director_domain::ids::{AgentId, SessionId, TaskId};
use director_domain::providers::{AgentProvider, Provider, SessionProvider, TaskProvider};
use director_domain::session::{AgentSession, SessionEnd, SessionStatus};
use director_domain::task::{Task, TaskStatus};

use crate::handoff::mapping::{agent_from_wire, session_from_wire, task_from_wire, task_to_wire};
use crate::handoff::transport::McpTransport;
use crate::handoff::wire::{AgentList, SessionSummary, TaskData, TaskTree};

/// Errors the handoff adapter can produce.
///
/// Deliberately a type of its own rather than reusing `ProviderError`: the
/// failures here are substrate-specific (a tool rejected the call, a reply
/// could not be parsed) and folding them into a generic vocabulary would lose
/// the message that explains how to fix the request.
#[derive(Debug, thiserror::Error)]
pub enum HandoffAdapterError {
    /// The substrate could not be reached or replied with malformed JSON-RPC.
    #[error("handoff-mcp transport failed: {0}")]
    Transport(#[from] crate::handoff::transport::TransportError),
    /// The substrate ran the tool and it reported failure (`isError`).
    #[error("handoff-mcp tool '{tool}' failed: {message}")]
    Tool {
        /// The tool that was called.
        tool: &'static str,
        /// The message the substrate returned.
        message: String,
    },
    /// A reply from the substrate could not be parsed into the shape Director
    /// expects — a sign the wire mirror in [`crate::handoff::wire`] has drifted
    /// from the live server.
    #[error("could not parse handoff-mcp's reply to '{tool}': {message}")]
    Malformed {
        /// The tool that was called.
        tool: &'static str,
        /// Why parsing failed.
        message: String,
    },
    /// The substrate has no way to do what was asked (e.g. setting an agent's
    /// status directly). Returned rather than approximating the call.
    #[error("the handoff-mcp substrate cannot do this: {0}")]
    Unsupported(String),
    /// A task Director asked for is not in the substrate.
    #[error("no such task in handoff-mcp: {0}")]
    NotFound(String),
}

/// A connection that can call handoff-mcp tools.
///
/// One method, so that [`McpTransport`] and a test fake are interchangeable.
/// The identity accessor is part of the seam because the substrate's
/// process-global agent identity constrains which agent operations are even
/// possible (see [`transport`] docs): the adapter must compare against it
/// rather than assume it can register an arbitrary agent.
#[async_trait]
pub trait HandoffWire: Send {
    /// Call `name` with `arguments`, returning the tool's text content.
    async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<String, HandoffAdapterError>;

    /// The agent identity this connection speaks as.
    fn agent_identity(&self) -> AgentId;
}

#[async_trait]
impl HandoffWire for McpTransport {
    async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<String, HandoffAdapterError> {
        // Fully-qualified so the inherent method resolves over the trait one.
        McpTransport::call_tool(self, name, arguments)
            .await
            .map_err(HandoffAdapterError::from)
    }

    fn agent_identity(&self) -> AgentId {
        McpTransport::agent_id(self).clone()
    }
}

/// Director's adapter for the handoff-mcp substrate.
///
/// Generic over the wire connection so tests can drive it with a fake; the
/// default is the real stdio transport.
pub struct HandoffAdapter<T = McpTransport> {
    /// The substrate connection. Mutex'd because a stdin/stdout pair is mutated
    /// by every call, and the provider traits are `&self`.
    transport: Mutex<T>,
    /// Absolute project path, sent with every call. The substrate resolves
    /// projects from arguments, never from the child's working directory.
    project_dir: PathBuf,
    /// Project name, used only at initialization.
    project_name: String,
    /// Task ids Director itself moved to `Done`. The one place the verification
    /// rule can be decided, because the substrate will not say. A standard
    /// mutex: it is only ever held across plain memory access, never an await.
    trusted_done_ids: std::sync::Mutex<HashSet<String>>,
    /// The identity every call is made as, captured at construction so reading
    /// it never has to touch the transport.
    agent_id: AgentId,
}

impl<T: HandoffWire> HandoffAdapter<T> {
    /// Wrap an already-connected transport.
    ///
    /// Does no I/O. [`HandoffAdapter::ensure_project`] is a separate call so a
    /// test can construct the adapter and arrange state through the fake first.
    pub fn new(
        transport: T,
        project_dir: impl Into<PathBuf>,
        project_name: impl Into<String>,
    ) -> Self {
        let agent_id = transport.agent_identity();
        HandoffAdapter {
            transport: Mutex::new(transport),
            project_dir: project_dir.into(),
            project_name: project_name.into(),
            trusted_done_ids: std::sync::Mutex::new(HashSet::new()),
            agent_id,
        }
    }

    /// The agent identity this adapter speaks as.
    pub fn agent_identity(&self) -> AgentId {
        self.agent_id.clone()
    }

    /// Call a handoff-mcp tool directly, bypassing the mapping.
    ///
    /// This is not part of any provider trait and never will be: it exists so
    /// a live integration test can reproduce substrate-level writes Director
    /// itself would never make — an agent ticking its own `done`, or a config
    /// change made out of band — and then observe how the adapter responds.
    /// Director's own code goes through the trait methods.
    pub async fn raw_call(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<String, HandoffAdapterError> {
        let mut transport = self.transport.lock().await;
        transport.call_tool(name, arguments).await
    }

    /// Initialize the substrate project and relax the settings that would
    /// otherwise reject Director's writes.
    ///
    /// Safe to call on every connection: `handoff_init` is only invoked for a
    /// project that has no `.handoff/` yet, and the config write is idempotent.
    pub async fn ensure_project(&self) -> Result<(), HandoffAdapterError> {
        match self
            .call(
                "handoff_init",
                json!({
                    "project_dir": self.project_dir_str(),
                    "project_name": self.project_name,
                    "description": MANAGED_BY,
                }),
            )
            .await
        {
            Ok(_) => {}
            // The tool refuses when .handoff/ already exists — exactly the
            // already-initialized case, not a failure to report.
            Err(HandoffAdapterError::Tool { message, .. })
                if message.contains("already exists") => {}
            Err(error) => return Err(error),
        }

        // Director deliberately never writes estimate_hours (it is a human
        // scheduling estimate Director has no basis to invent), but the
        // substrate rejects any in_progress/review/done write without one.
        // Since Director maps VerificationPending to in_progress and Done to
        // done, that rule would block almost every status write. Disabling it
        // in Director's own integration project is the honest fix; the
        // alternative — a fabricated hour count — would corrupt the
        // substrate's metrics.
        self.call(
            "handoff_update_config",
            json!({
                "project_dir": self.project_dir_str(),
                "updates": { "settings": { "require_estimate_hours": false } },
            }),
        )
        .await?;
        Ok(())
    }

    fn project_dir_str(&self) -> String {
        self.project_dir.to_string_lossy().into_owned()
    }

    /// Call a tool and return its raw text reply.
    async fn call(
        &self,
        tool: &'static str,
        mut arguments: Value,
    ) -> Result<String, HandoffAdapterError> {
        // Every tool except init/load_context requires project_dir; sending it
        // uniformly is simpler than a per-tool rule and harmless to the two
        // that ignore it.
        if let Some(obj) = arguments.as_object_mut() {
            obj.entry("project_dir")
                .or_insert(Value::String(self.project_dir_str()));
        }
        let mut transport = self.transport.lock().await;
        transport
            .call_tool(tool, arguments)
            .await
            .map_err(|error| match error {
                // A tool-level failure keeps the tool name so the caller knows
                // which call to fix; everything else propagates as-is.
                HandoffAdapterError::Tool { message, .. } => {
                    HandoffAdapterError::Tool { tool, message }
                }
                other => other,
            })
    }

    /// Call a tool and parse its text reply as JSON.
    async fn call_json<R: DeserializeOwned>(
        &self,
        tool: &'static str,
        arguments: Value,
    ) -> Result<R, HandoffAdapterError> {
        let text = self.call(tool, arguments).await?;
        serde_json::from_str::<R>(&text).map_err(|message| HandoffAdapterError::Malformed {
            tool,
            message: format!(
                "{message}: {}",
                // The reply that failed to parse is the best diagnostic the
                // substrate gives us; truncate so a runaway reply cannot flood.
                text.chars().take(512).collect::<String>()
            ),
        })
    }

    /// Read one task's full record, or `None` if the substrate has no such task.
    async fn fetch_task(&self, id: &str) -> Result<Option<TaskData>, HandoffAdapterError> {
        match self
            .call_json::<TaskData>(
                "handoff_get_task",
                json!({ "task_id": id, "include_dependents": false }),
            )
            .await
        {
            Ok(data) => Ok(Some(data)),
            Err(HandoffAdapterError::Tool { message, .. })
                // The substrate has no not-found reply code; it sends a
                // "Task not found: '...'" message instead. Matching on it is a
                // string contract with the substrate, recorded here so the
                // fragility is visible rather than buried.
                if message.contains("not found") =>
            {
                Ok(None)
            }
            Err(other) => Err(other),
        }
    }

    /// Map a substrate record through the trusted-done set.
    fn to_task(&self, data: TaskData) -> Task {
        let trusted = self
            .trusted_done_ids
            .lock()
            .expect("trusted_done_ids poisoned");
        task_from_wire(&data, &trusted)
    }

    /// Record that Director itself completed this task.
    fn trust_done(&self, id: &str) {
        let mut trusted = self
            .trusted_done_ids
            .lock()
            .expect("trusted_done_ids poisoned");
        trusted.insert(id.to_string());
    }

    /// Write a task and return the record the substrate actually stored.
    async fn write_task(&self, task: &Task) -> Result<Task, HandoffAdapterError> {
        let mut wire = task_to_wire(task);
        // The substrate rejects a done transition unless every criterion is
        // checked. Director reaches Done only through its own verification, so
        // ticking the criteria here records that verdict — it is not
        // rubber-stamping an agent's claim, which never reaches this method.
        if task.status == TaskStatus::Done {
            for criterion in &mut wire.done_criteria {
                criterion.checked = true;
            }
        }

        // The reply is prose; the canonical record has to be read back.
        self.call(
            "handoff_update_task",
            json!({ "task": serde_json::to_value(&wire).expect("task serializes") }),
        )
        .await?;

        let stored = self
            .fetch_task(task.id.as_str())
            .await?
            .ok_or_else(|| HandoffAdapterError::NotFound(task.id.to_string()))?;

        if task.status == TaskStatus::Done {
            self.trust_done(task.id.as_str());
        }
        Ok(self.to_task(stored))
    }

    /// Read a task as Director's, applying the trusted-done set.
    async fn read_task(&self, id: &TaskId) -> Result<Option<Task>, HandoffAdapterError> {
        Ok(self.fetch_task(id.as_str()).await?.map(|d| self.to_task(d)))
    }

    /// The agent record for `id` as Director sees it, or `None`.
    ///
    /// `include_tasks` is what makes an `Active` agent resolvable to `Busy`:
    /// the substrate's status spans both and only the claimed-task list
    /// separates them.
    async fn agent_record(&self, id: &AgentId) -> Result<Option<Agent>, HandoffAdapterError> {
        let list: AgentList = self
            .call_json("handoff_list_agents", json!({ "include_tasks": true }))
            .await?;
        Ok(list
            .agents
            .iter()
            .find(|record| record.agent_id == id.as_str())
            .map(agent_from_wire))
    }
}

/// Marker written into the substrate's project description, so a human
/// inspecting `.handoff/config.toml` can see that this project is managed.
const MANAGED_BY: &str = "Managed by director-brain.";

impl HandoffAdapter<McpTransport> {
    /// Spawn the server and connect.
    ///
    /// Bakes `agent_id` into the child's environment (the substrate holds one
    /// process-wide identity — see [`transport`]), initializes the project, and
    /// relaxes the estimate setting. One adapter instance therefore speaks as
    /// exactly one agent; multiplexing several is Phase 8's pool.
    pub async fn connect(
        binary: &str,
        agent_id: AgentId,
        project_dir: impl Into<PathBuf>,
        project_name: impl Into<String>,
    ) -> Result<Self, HandoffAdapterError> {
        let project_dir = project_dir.into();
        let transport = McpTransport::spawn(binary, agent_id, &project_dir).await?;
        let adapter = HandoffAdapter::new(transport, project_dir, project_name);
        adapter.ensure_project().await?;
        Ok(adapter)
    }
}

/// Every task id in a tree, in tree order.
fn tree_ids(tree: &TaskTree) -> Vec<String> {
    let mut ids = Vec::new();
    for node in &tree.task_tree {
        collect_ids(node, &mut ids);
    }
    ids
}

fn collect_ids(node: &crate::handoff::wire::TaskTreeNode, ids: &mut Vec<String>) {
    ids.push(node.id.clone());
    for child in &node.children {
        collect_ids(child, ids);
    }
}

#[async_trait]
impl<T: HandoffWire> Provider for HandoffAdapter<T> {
    type Error = HandoffAdapterError;
}

#[async_trait]
impl<T: HandoffWire> TaskProvider for HandoffAdapter<T> {
    async fn create_task(&self, task: Task) -> Result<Task, Self::Error> {
        // Director controls the id (the substrate's update tool is an upsert),
        // so the id round-trips rather than being renamed by the substrate.
        self.write_task(&task).await
    }

    async fn get_task(&self, id: &TaskId) -> Result<Option<Task>, Self::Error> {
        self.read_task(id).await
    }

    async fn list_tasks(&self) -> Result<Vec<Task>, Self::Error> {
        // list_tasks answers with summaries only; full records need a fetch
        // per task. The N+1 is the substrate's shape, not a choice.
        let tree: TaskTree = self.call_json("handoff_list_tasks", json!({})).await?;
        let mut tasks = Vec::with_capacity(tree.task_tree.len());
        for id in tree_ids(&tree) {
            if let Some(data) = self.fetch_task(&id).await? {
                tasks.push(self.to_task(data));
            }
        }
        Ok(tasks)
    }

    async fn update_task(&self, task: Task) -> Result<Task, Self::Error> {
        self.write_task(&task).await
    }

    async fn set_task_status(&self, id: &TaskId, status: TaskStatus) -> Result<(), Self::Error> {
        // Load-modify-store, so a status change cannot clobber the rest of the
        // record the way a full write would.
        let Some(mut task) = self.read_task(id).await? else {
            return Err(HandoffAdapterError::NotFound(id.to_string()));
        };
        if task.status == status {
            return Ok(());
        }
        task.status = status;
        self.write_task(&task).await?;
        Ok(())
    }

    async fn ready_tasks(&self) -> Result<Vec<Task>, Self::Error> {
        let tasks = self.list_tasks().await?;
        let statuses: HashMap<TaskId, TaskStatus> = tasks
            .iter()
            .map(|task| (task.id.clone(), task.status))
            .collect();
        Ok(tasks
            .into_iter()
            .filter(|task| task.is_ready_given(|dependency| statuses.get(dependency).copied()))
            .collect())
    }

    async fn add_dependency(&self, task: &TaskId, dependency: &TaskId) -> Result<(), Self::Error> {
        let Some(mut director_task) = self.read_task(task).await? else {
            return Err(HandoffAdapterError::NotFound(task.to_string()));
        };
        if director_task.dependencies.contains(dependency) {
            return Ok(());
        }
        director_task.dependencies.push(dependency.clone());
        self.write_task(&director_task).await?;
        Ok(())
    }
}

#[async_trait]
impl<T: HandoffWire> AgentProvider for HandoffAdapter<T> {
    async fn register_agent(&self, agent: Agent) -> Result<Agent, Self::Error> {
        self.require_own_identity(&agent.id).await?;
        // The substrate has no register tool: the record is created (or a
        // reconnecting identity's record refreshed) as a side effect of
        // loading context, using the process-wide identity.
        self.call("handoff_load_context", json!({})).await?;
        self.agent_record(&agent.id)
            .await?
            .ok_or_else(|| HandoffAdapterError::Malformed {
                tool: "handoff_list_agents",
                message: "load_context did not register the agent".into(),
            })
    }

    async fn get_agent(&self, id: &AgentId) -> Result<Option<Agent>, Self::Error> {
        self.agent_record(id).await
    }

    async fn list_agents(&self) -> Result<Vec<Agent>, Self::Error> {
        let list: AgentList = self
            .call_json("handoff_list_agents", json!({ "include_tasks": true }))
            .await?;
        Ok(list.agents.iter().map(agent_from_wire).collect())
    }

    async fn heartbeat(&self, id: &AgentId) -> Result<AgentStatus, Self::Error> {
        self.require_own_identity(id).await?;
        // The substrate has no heartbeat tool either; load_context refreshes
        // last_heartbeat and recomputes status on read.
        self.call("handoff_load_context", json!({})).await?;
        let agent = self
            .agent_record(id)
            .await?
            .ok_or_else(|| HandoffAdapterError::Malformed {
                tool: "handoff_list_agents",
                message: "heartbeat did not refresh the agent".into(),
            })?;
        Ok(agent.status)
    }

    async fn set_agent_status(
        &self,
        _id: &AgentId,
        _status: AgentStatus,
    ) -> Result<(), Self::Error> {
        // The substrate's agent statuses are derived from heartbeat age on
        // every read, so there is nothing to write. An operator taking an agent
        // offline is a Director-owned fact for Director's own store (Phase 5),
        // not something to approximate into a heartbeat-derived field.
        Err(HandoffAdapterError::Unsupported(
            "handoff-mcp derives agent status from heartbeat age and cannot be set directly".into(),
        ))
    }

    async fn available_agents(&self) -> Result<Vec<Agent>, Self::Error> {
        Ok(self
            .list_agents()
            .await?
            .into_iter()
            .filter(|agent| agent.status == AgentStatus::Available)
            .collect())
    }
}

impl<T: HandoffWire> HandoffAdapter<T> {
    /// Refuse an agent operation aimed at anyone but this connection's
    /// process-wide identity, which is all the substrate can act as.
    async fn require_own_identity(&self, id: &AgentId) -> Result<(), HandoffAdapterError> {
        if &self.agent_id != id {
            return Err(HandoffAdapterError::Unsupported(format!(
                "this handoff-mcp connection speaks as '{}', not '{}'",
                self.agent_id, id
            )));
        }
        Ok(())
    }
}

/// The substrate's session files carry the task list under this key.
const RELATED_TASKS_KEY: &str = "related_task_ids";

#[async_trait]
impl<T: HandoffWire> SessionProvider for HandoffAdapter<T> {
    async fn start_session(&self, session: AgentSession) -> Result<AgentSession, Self::Error> {
        // save_context with session_status "active" is the substrate's
        // session-start call. The substrate assigns the id itself — unlike task
        // ids, session ids are not caller-controllable — so the session this
        // returns is the one the substrate created, not the id passed in.
        self.call(
            "handoff_save_context",
            json!({
                "summary": session_summary_for(&session),
                "session_status": "active",
                RELATED_TASKS_KEY: session
                    .task_id
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>(),
            }),
        )
        .await?;
        self.active_session().await
    }

    async fn get_session(&self, id: &SessionId) -> Result<Option<AgentSession>, Self::Error> {
        let sessions: Vec<SessionSummary> = self.list_session_summaries().await?;
        Ok(sessions
            .into_iter()
            .find(|summary| summary.id == id.as_str())
            .map(|summary| session_from_wire(&summary)))
    }

    async fn close_session(&self, id: &SessionId, end: SessionEnd) -> Result<(), Self::Error> {
        // The substrate distinguishes only closed vs paused. Director's richer
        // reasons map to the nearest terminal state; the real reason is
        // Director's to keep (Phase 5), because the substrate has no field
        // for it.
        let pause = matches!(end, SessionEnd::ContextExhausted);
        let mut arguments = serde_json::Map::new();
        arguments.insert("summary".into(), Value::String(session_end_summary(end)));
        if pause {
            arguments.insert("pause_session_id".into(), Value::String(id.to_string()));
        } else {
            arguments.insert("close_session_id".into(), Value::String(id.to_string()));
        }
        self.call("handoff_save_context", Value::Object(arguments))
            .await?;
        Ok(())
    }

    async fn sessions_for_task(&self, task: &TaskId) -> Result<Vec<AgentSession>, Self::Error> {
        // Session summaries do not carry their task list, so each candidate
        // session has to be fetched to read it.
        let summaries = self.list_session_summaries().await?;
        let mut sessions = Vec::new();
        for summary in summaries {
            let detail: Value = self
                .call_json("handoff_get_session", json!({ "session_id": summary.id }))
                .await?;
            let mut related = detail
                .get(RELATED_TASKS_KEY)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str);
            if related.any(|id| id == task.as_str()) {
                sessions.push(session_from_wire(&summary));
            }
        }
        // Summaries come back newest-first; a task's history reads better in
        // the order the work happened.
        sessions.reverse();
        Ok(sessions)
    }

    async fn fork_session(
        &self,
        parent: &SessionId,
        new_id: SessionId,
    ) -> Result<AgentSession, Self::Error> {
        // The substrate assigns the forked id; the lineage it reports back is
        // what Director keeps. `new_id` is accepted for the trait's signature
        // and recorded in Director's own store (Phase 5), where ids Director
        // issued are meaningful.
        let result: Value = self
            .call_json(
                "handoff_fork_session",
                json!({
                    "source_session_id": parent.to_string(),
                    "summary": format!("Forked by director-brain as {}", new_id),
                }),
            )
            .await?;
        let forked_id = result
            .get("session_id")
            .and_then(Value::as_str)
            .ok_or_else(|| HandoffAdapterError::Malformed {
                tool: "handoff_fork_session",
                message: "no session_id in the reply".into(),
            })?;
        self.get_session(&SessionId::from_string(forked_id.to_string()))
            .await?
            .ok_or_else(|| HandoffAdapterError::NotFound(forked_id.to_string()))
    }
}

impl<T: HandoffWire> HandoffAdapter<T> {
    /// Every session summary the substrate holds.
    ///
    /// The substrate returns a bare JSON array rather than an envelope.
    async fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, HandoffAdapterError> {
        self.call_json::<Vec<SessionSummary>>("handoff_list_sessions", json!({}))
            .await
    }

    /// The session the substrate currently has active for this connection,
    /// after a start or a save.
    async fn active_session(&self) -> Result<AgentSession, HandoffAdapterError> {
        let summaries = self.list_session_summaries().await?;
        summaries
            .into_iter()
            .find(|summary| session_from_wire(summary).status == SessionStatus::Active)
            .map(|summary| session_from_wire(&summary))
            .ok_or_else(|| HandoffAdapterError::Malformed {
                tool: "handoff_save_context",
                message: "no active session after starting one".into(),
            })
    }
}

/// A one-line summary of a session for the substrate's `summary` field,
/// derived from what Director actually knows about it.
fn session_summary_for(session: &AgentSession) -> String {
    match session.task_id.as_ref() {
        Some(task) => format!("Working on {}", task),
        None => "Session started by director-brain".to_string(),
    }
}

/// A short record of why a session closed, for the substrate's `summary`.
fn session_end_summary(end: SessionEnd) -> String {
    match end {
        SessionEnd::Clean => "Closed cleanly".to_string(),
        SessionEnd::Vanished => "Heartbeat stopped; closed by director-brain".to_string(),
        SessionEnd::Terminated => "Terminated by director-brain".to_string(),
        SessionEnd::ContextExhausted => "Context window exhausted; paused".to_string(),
        SessionEnd::Forked => "Forked".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use director_domain::agent::Harness;
    use director_domain::ids::{MachineId, TaskId};

    /// A fake substrate: a map from tool name to canned reply.
    ///
    /// It records what the adapter sent, so a test asserts on the actual
    /// request rather than on what the adapter claims it would do. Replies are
    /// functions of the arguments, because the interesting adapter behavior —
    /// the write repairs, the trusted-done set — depends on what was sent.
    struct FakeHandoff {
        agent_id: AgentId,
        tasks: HashMap<String, TaskData>,
        sessions: Vec<Value>,
        /// Every call, in order: (tool, arguments).
        calls: Vec<(String, Value)>,
    }

    impl FakeHandoff {
        fn new(agent_id: &str) -> Self {
            FakeHandoff {
                agent_id: AgentId::from_string(agent_id.to_string()),
                tasks: HashMap::new(),
                sessions: Vec::new(),
                calls: Vec::new(),
            }
        }

        fn task(&self, id: &str) -> TaskData {
            self.tasks.get(id).cloned().unwrap_or_else(|| TaskData {
                id: id.to_string(),
                title: format!("Task {id}"),
                status: "todo".to_string(),
                notes: None,
                priority: None,
                created_at: "2026-09-25T10:00:00+00:00".to_string(),
                updated_at: "2026-09-25T10:00:00+00:00".to_string(),
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
            })
        }
    }

    #[async_trait]
    impl HandoffWire for std::sync::Mutex<FakeHandoff> {
        async fn call_tool(
            &mut self,
            name: &str,
            arguments: Value,
        ) -> Result<String, HandoffAdapterError> {
            // Exclusive access is already held (&mut self), so get_mut
            // reaches the state without taking the lock.
            let fake = self.get_mut().expect("fake poisoned");
            fake.calls.push((name.to_string(), arguments.clone()));

            match name {
                "handoff_init" | "handoff_update_config" => Ok("ok".to_string()),
                "handoff_get_task" => {
                    let id = arguments
                        .get("task_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if fake.tasks.contains_key(id) {
                        serde_json::to_string_pretty(&fake.task(id)).map_err(|e| {
                            HandoffAdapterError::Malformed {
                                tool: "handoff_get_task",
                                message: e.to_string(),
                            }
                        })
                    } else {
                        // Mirrors the substrate's real not-found reply.
                        Err(HandoffAdapterError::Tool {
                            tool: "handoff_get_task",
                            message: format!("Task not found: '{id}'."),
                        })
                    }
                }
                "handoff_list_tasks" => {
                    let ids: Vec<String> = fake.tasks.keys().cloned().collect();
                    let tree = serde_json::json!({
                        "task_tree": ids.iter().map(|id| serde_json::json!({
                            "id": id, "title": fake.task(id).title, "status": fake.task(id).status,
                        })).collect::<Vec<_>>(),
                        "task_summary": {"total": ids.len()},
                    });
                    serde_json::to_string_pretty(&tree).map_err(|e| {
                        HandoffAdapterError::Malformed {
                            tool: "handoff_list_tasks",
                            message: e.to_string(),
                        }
                    })
                }
                "handoff_update_task" => {
                    let task = arguments.get("task").expect("update_task needs a task");
                    let id = task.get("id").and_then(Value::as_str).unwrap_or("t1");
                    let mut data = fake.task(id);
                    if let Some(title) = task.get("title").and_then(Value::as_str) {
                        data.title = title.to_string();
                    }
                    if let Some(status) = task.get("status").and_then(Value::as_str) {
                        data.status = status.to_string();
                    }
                    if let Some(criteria) = task.get("done_criteria") {
                        data.done_criteria = criteria
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|c| crate::handoff::wire::DoneCriterion {
                                item: c
                                    .get("item")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                checked: c.get("checked").and_then(Value::as_bool).unwrap_or(false),
                            })
                            .collect();
                    }
                    if let Some(deps) = task.get("dependencies") {
                        data.dependencies = deps
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .map(String::from)
                            .collect();
                    }
                    fake.tasks.insert(id.to_string(), data);
                    Ok(format!("Updated task {id}"))
                }
                "handoff_list_agents" => {
                    let agents = serde_json::json!({
                        "agents": [{
                            "agent_id": fake.agent_id.as_str(),
                            "session_id": "sess-1",
                            "worktree": "C:/repo",
                            "branch": "main",
                            "status": "active",
                            "registered_at": "2026-09-25T10:00:00+00:00",
                            "last_heartbeat": "2026-09-25T10:05:00+00:00",
                            "claimed_tasks": ["AUTH-42"],
                        }],
                        "total": 1,
                    });
                    serde_json::to_string_pretty(&agents).map_err(|e| {
                        HandoffAdapterError::Malformed {
                            tool: "handoff_list_agents",
                            message: e.to_string(),
                        }
                    })
                }
                "handoff_load_context" => {
                    Ok(serde_json::json!({ "agent_id": fake.agent_id.as_str() }).to_string())
                }
                "handoff_list_sessions" => {
                    serde_json::to_string_pretty(&fake.sessions).map_err(|e| {
                        HandoffAdapterError::Malformed {
                            tool: "handoff_list_sessions",
                            message: e.to_string(),
                        }
                    })
                }
                "handoff_save_context" => {
                    // Record an active session mirroring the substrate's shape.
                    let id = format!("sess-{}", fake.sessions.len() + 1);
                    let mut session = serde_json::json!({
                        "id": id,
                        "status": if arguments.get("session_status").and_then(Value::as_str) == Some("active") {
                            "active"
                        } else {
                            "closed"
                        },
                        "summary": arguments.get("summary").and_then(Value::as_str).unwrap_or(""),
                        "agent_id": fake.agent_id.as_str(),
                        "worktree": "C:/repo",
                        "started_at": "2026-09-25T10:00:00+00:00",
                        "related_task_ids": arguments.get(RELATED_TASKS_KEY).cloned().unwrap_or(Value::Array(vec![])),
                    });
                    if let Some(pause) = arguments.get("pause_session_id").and_then(Value::as_str) {
                        session["status"] = Value::String("paused".to_string());
                        session["id"] = Value::String(pause.to_string());
                    }
                    if let Some(close) = arguments.get("close_session_id").and_then(Value::as_str) {
                        session["id"] = Value::String(close.to_string());
                        session["status"] = Value::String("closed".to_string());
                        session["ended_at"] =
                            Value::String("2026-09-25T11:00:00+00:00".to_string());
                    }
                    // One record per session id, as the substrate keeps one
                    // file per id: a close or a pause replaces the record
                    // rather than adding a second view of the same session.
                    let id = session
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_default();
                    if let Some(existing) = fake
                        .sessions
                        .iter_mut()
                        .find(|s| s.get("id").and_then(Value::as_str) == Some(&id))
                    {
                        *existing = session;
                    } else {
                        fake.sessions.push(session);
                    }
                    Ok("saved".to_string())
                }
                "handoff_get_session" => {
                    let id = arguments
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    fake.sessions
                        .iter()
                        .find(|s| s.get("id").and_then(Value::as_str) == Some(id))
                        .cloned()
                        .map(|s| serde_json::to_string_pretty(&s).expect("serializes"))
                        .ok_or_else(|| HandoffAdapterError::Tool {
                            tool: "handoff_get_session",
                            message: format!("Session not found: {id}."),
                        })
                }
                "handoff_fork_session" => {
                    let parent = arguments.get("source_session_id").and_then(Value::as_str);
                    let agent_id = fake.agent_id.as_str().to_string();
                    let id = "forked-1";
                    fake.sessions.push(serde_json::json!({
                        "id": id,
                        "status": "active",
                        "summary": "forked",
                        "parent_session_id": parent,
                        "agent_id": agent_id,
                        "worktree": "C:/repo",
                    }));
                    Ok(serde_json::json!({
                        "session_id": id,
                        "parent_session_id": parent,
                        "status": "active",
                    })
                    .to_string())
                }
                other => Err(HandoffAdapterError::Unsupported(format!(
                    "fake has no {other}"
                ))),
            }
        }

        fn agent_identity(&self) -> AgentId {
            self.lock().expect("fake poisoned").agent_id.clone()
        }
    }

    fn adapter(agent_id: &str) -> HandoffAdapter<std::sync::Mutex<FakeHandoff>> {
        HandoffAdapter::new(
            std::sync::Mutex::new(FakeHandoff::new(agent_id)),
            "C:/repo",
            "test-project",
        )
    }

    fn task(id: &str, status: TaskStatus) -> Task {
        let mut task = Task::new(
            TaskId::from_string(id.to_string()),
            format!("Task {id}"),
            "objective",
        );
        task.status = status;
        task
    }

    #[tokio::test]
    async fn creating_a_task_round_trips_its_id_and_status() {
        let adapter = adapter("AGENT-1");
        let created = adapter
            .create_task(task("AUTH-1", TaskStatus::Todo))
            .await
            .expect("create");

        assert_eq!(created.id, TaskId::from_string("AUTH-1".to_string()));
        assert_eq!(created.status, TaskStatus::Todo);
        assert_eq!(created.title, "Task AUTH-1");

        let fetched = adapter
            .get_task(&TaskId::from_string("AUTH-1".to_string()))
            .await
            .expect("get");
        assert_eq!(fetched.map(|t| t.id), Some(created.id));
    }

    #[tokio::test]
    async fn get_task_returns_none_when_the_substrate_has_no_such_task() {
        let adapter = adapter("AGENT-1");
        assert!(adapter
            .get_task(&TaskId::from_string("NOPE-9".to_string()))
            .await
            .expect("get of a missing task should not error")
            .is_none());
    }

    #[tokio::test]
    async fn an_agent_reported_done_does_not_complete_the_task() {
        // The acceptance criterion at the adapter boundary: the substrate says
        // `done`, but nothing verified it, so Director must not see Done.
        let adapter = adapter("AGENT-1");
        adapter
            .create_task(task("AUTH-2", TaskStatus::Todo))
            .await
            .expect("create");

        // Simulate the agent ticking its own checkbox: write `done` straight
        // through the fake, bypassing Director's write path.
        {
            let transport = adapter.transport.lock().await;
            let mut fake = transport.lock().unwrap();
            let mut data = fake.task("AUTH-2");
            data.status = "done".to_string();
            fake.tasks.insert("AUTH-2".to_string(), data);
        }

        let observed = adapter
            .get_task(&TaskId::from_string("AUTH-2".to_string()))
            .await
            .expect("get")
            .expect("task exists");
        assert_eq!(
            observed.status,
            TaskStatus::VerificationPending,
            "an agent's done must not become Director's Done"
        );
    }

    #[tokio::test]
    async fn a_director_completion_comes_back_as_done() {
        // When Director itself moves the task to Done, the trusted-done set
        // makes the read agree — without any help from the substrate, which
        // cannot tell Director's done from an agent's.
        let adapter = adapter("AGENT-1");
        adapter
            .create_task(task("AUTH-3", TaskStatus::Todo))
            .await
            .expect("create");

        adapter
            .set_task_status(&TaskId::from_string("AUTH-3".to_string()), TaskStatus::Done)
            .await
            .expect("set status");

        let observed = adapter
            .get_task(&TaskId::from_string("AUTH-3".to_string()))
            .await
            .expect("get")
            .expect("task exists");
        assert_eq!(observed.status, TaskStatus::Done);
    }

    #[tokio::test]
    async fn completing_a_task_ticks_its_criteria_on_the_wire() {
        // The substrate rejects a done transition with an unchecked criterion,
        // so Director's completion must check them. Asserting on the request
        // shows the repair happened on the wire, where the rule is enforced.
        let adapter = adapter("AGENT-1");
        let mut task = task("AUTH-4", TaskStatus::Todo);
        task.expected_outputs
            .push(director_domain::task::ExpectedOutput {
                criterion: "login returns 200".into(),
                check: None,
            });
        adapter.create_task(task).await.expect("create");

        adapter
            .set_task_status(&TaskId::from_string("AUTH-4".to_string()), TaskStatus::Done)
            .await
            .expect("complete");

        let transport = adapter.transport.lock().await;
        let fake = transport.lock().unwrap();
        let update = fake
            .calls
            .iter()
            .rev()
            .find(|(tool, _)| tool == "handoff_update_task")
            .expect("an update was sent");
        let criteria = update
            .1
            .get("task")
            .and_then(|t| t.get("done_criteria"))
            .and_then(Value::as_array)
            .expect("criteria were sent");
        assert!(
            criteria
                .iter()
                .all(|c| c.get("checked").and_then(Value::as_bool).unwrap_or(false)),
            "every criterion was checked"
        );
    }

    #[tokio::test]
    async fn ensure_project_is_idempotent_across_repeated_calls() {
        let adapter = adapter("AGENT-1");
        adapter.ensure_project().await.expect("first");
        // The second init would be refused by the substrate's "already exists"
        // guard; the adapter treats that as success.
        adapter.ensure_project().await.expect("second");
    }

    #[tokio::test]
    async fn list_tasks_reflects_what_was_created() {
        let adapter = adapter("AGENT-1");
        adapter
            .create_task(task("AUTH-5", TaskStatus::InProgress))
            .await
            .expect("create");
        adapter
            .create_task(task("AUTH-6", TaskStatus::Todo))
            .await
            .expect("create");

        let ids: Vec<String> = adapter
            .list_tasks()
            .await
            .expect("list")
            .into_iter()
            .map(|t| t.id.to_string())
            .collect();
        assert!(ids.contains(&"AUTH-5".to_string()));
        assert!(ids.contains(&"AUTH-6".to_string()));
    }

    #[tokio::test]
    async fn a_dependency_added_twice_is_stored_once() {
        let adapter = adapter("AGENT-1");
        adapter
            .create_task(task("AUTH-7", TaskStatus::Todo))
            .await
            .expect("create");
        adapter
            .create_task(task("AUTH-8", TaskStatus::Todo))
            .await
            .expect("create");

        adapter
            .add_dependency(
                &TaskId::from_string("AUTH-7".to_string()),
                &TaskId::from_string("AUTH-8".to_string()),
            )
            .await
            .expect("add");
        // Idempotent: the substrate would otherwise accumulate duplicates.
        adapter
            .add_dependency(
                &TaskId::from_string("AUTH-7".to_string()),
                &TaskId::from_string("AUTH-8".to_string()),
            )
            .await
            .expect("add again");

        let task = adapter
            .get_task(&TaskId::from_string("AUTH-7".to_string()))
            .await
            .expect("get")
            .expect("task exists");
        assert_eq!(task.dependencies.len(), 1);
    }

    #[tokio::test]
    async fn agent_operations_are_refused_for_another_identity() {
        // The substrate holds one process-wide identity; asking it to act as
        // anyone else must fail loudly rather than misattribute the work.
        let adapter = adapter("AGENT-1");
        let other = Agent::register(
            AgentId::from_string("AGENT-2".to_string()),
            "two",
            Harness::Other("test".into()),
            MachineId::from_string("MACH-x".to_string()),
            vec![],
        );
        let error = adapter.register_agent(other).await.unwrap_err();
        assert!(matches!(error, HandoffAdapterError::Unsupported(_)));
    }

    #[tokio::test]
    async fn the_connections_own_agent_registers_and_heartbeats() {
        let adapter = adapter("AGENT-1");
        let agent = Agent::register(
            adapter.agent_identity(),
            "one",
            Harness::Other("test".into()),
            MachineId::from_string("MACH-x".to_string()),
            vec![],
        );

        // The fake reports an active agent holding a task, which is Director's
        // Busy — the ambiguity the claimed-task list exists to resolve.
        let registered = adapter.register_agent(agent).await.expect("register");
        assert_eq!(registered.id, AgentId::from_string("AGENT-1".to_string()));
        assert_eq!(registered.status, AgentStatus::Busy);

        let status = adapter
            .heartbeat(&AgentId::from_string("AGENT-1".to_string()))
            .await
            .expect("heartbeat");
        assert_eq!(status, AgentStatus::Busy);
    }

    #[tokio::test]
    async fn setting_an_agent_status_is_unsupported_not_approximated() {
        let adapter = adapter("AGENT-1");
        let error = adapter
            .set_agent_status(
                &AgentId::from_string("AGENT-1".to_string()),
                AgentStatus::Offline,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, HandoffAdapterError::Unsupported(_)));
    }

    #[tokio::test]
    async fn starting_a_session_records_its_task() {
        let adapter = adapter("AGENT-1");
        let session = AgentSession::start(
            SessionId::from_string("ignored".to_string()),
            adapter.agent_identity(),
            MachineId::from_string("MACH-x".to_string()),
            Some(TaskId::from_string("AUTH-9".to_string())),
        );

        // The substrate assigns the id, so the returned session's id is its
        // own — the point is that the task linkage survives.
        let started = adapter.start_session(session).await.expect("start");
        assert_eq!(started.agent_id, adapter.agent_identity());

        let for_task = adapter
            .sessions_for_task(&TaskId::from_string("AUTH-9".to_string()))
            .await
            .expect("sessions for task");
        assert_eq!(for_task.len(), 1, "the session is linked to its task");
        assert!(adapter
            .sessions_for_task(&TaskId::from_string("AUTH-1".to_string()))
            .await
            .expect("sessions for another task")
            .is_empty());
    }

    #[tokio::test]
    async fn closing_a_session_marks_it_closed_in_the_substrate() {
        let adapter = adapter("AGENT-1");
        let started = adapter
            .start_session(AgentSession::start(
                SessionId::from_string("ignored".to_string()),
                adapter.agent_identity(),
                MachineId::from_string("MACH-x".to_string()),
                None,
            ))
            .await
            .expect("start");

        adapter
            .close_session(&started.id, SessionEnd::Clean)
            .await
            .expect("close");

        let closed = adapter
            .get_session(&started.id)
            .await
            .expect("get")
            .expect("session exists");
        assert_eq!(closed.status, SessionStatus::Closed);
        assert_eq!(closed.end, Some(SessionEnd::Clean));
    }

    #[tokio::test]
    async fn forking_a_session_records_parent_lineage() {
        let adapter = adapter("AGENT-1");
        let parent = adapter
            .start_session(AgentSession::start(
                SessionId::from_string("ignored".to_string()),
                adapter.agent_identity(),
                MachineId::from_string("MACH-x".to_string()),
                None,
            ))
            .await
            .expect("start");

        let parent_id = parent.id.clone();
        let forked = adapter
            .fork_session(
                &parent.id,
                SessionId::from_string("director-issued".to_string()),
            )
            .await
            .expect("fork");

        assert_eq!(
            forked.parent_session_id,
            Some(parent_id),
            "the substrate's lineage is what Director keeps"
        );
        assert_ne!(forked.id, parent.id);
    }

    #[tokio::test]
    async fn an_unparsed_reply_is_reported_as_malformed_not_as_success() {
        // If the wire mirror drifts from the live server, the failure must be
        // loud. The fake serves a shape TaskData cannot parse.
        struct Broken;
        #[async_trait]
        impl HandoffWire for std::sync::Mutex<Broken> {
            async fn call_tool(
                &mut self,
                _name: &str,
                _arguments: Value,
            ) -> Result<String, HandoffAdapterError> {
                // No `id` and no `title`: TaskData requires both.
                Ok(r#"{"status": "todo"}"#.to_string())
            }
            fn agent_identity(&self) -> AgentId {
                AgentId::from_string("AGENT-1".to_string())
            }
        }
        let adapter = HandoffAdapter::<std::sync::Mutex<Broken>>::new(
            std::sync::Mutex::new(Broken),
            "C:/repo",
            "test-project",
        );
        assert!(matches!(
            adapter
                .get_task(&TaskId::from_string("AUTH-1".to_string()))
                .await,
            Err(HandoffAdapterError::Malformed { .. })
        ));
    }
}
