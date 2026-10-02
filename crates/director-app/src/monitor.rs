//! The MONITOR step of Orqyn's loop.
//!
//! MONITOR reads what ASSIGN started. Every task handed to an agent is a claim
//! on that agent's attention, and the claim is only as good as the evidence the
//! agent is still there — so this step looks at every task in flight, asks what
//! the holder's heartbeats say, and reclaims the leases whose holder has gone
//! quiet long enough that the work is not being done.
//!
//! ```text
//! OBSERVE → PLAN → ASSIGN → MONITOR → VERIFY → REPLAN
//!                            ▲
//!                            │
//!                    in_progress tasks
//!                    + their holders'
//!                      heartbeats
//! ```
//!
//! ## What MONITOR is responsible for, and what it is not
//!
//! MONITOR owns the *honesty of the in-flight picture*. It derives liveness
//! from heartbeat age, it expires leases that have run out, and it records the
//! sessions agents report when they start work. A round writes nothing about
//! tasks that are merely quiet — `Stale` is a report, not an intervention,
//! because a quiet agent may be thinking and a lease that ends early is work
//! that has to restart.
//!
//! MONITOR does **not** decide what to do with a task whose lease it expired.
//! Putting the task back to `todo` is as far as it goes: choosing the next agent
//! is ASSIGN's job, and choosing whether the work is still worth doing is
//! REPLAN's. It also does not decide that an agent is dead — it derives liveness
//! from the same 30/60-minute windows the substrate uses, and it reclaims; the
//! agent's own registry is what says what it is.
//!
//! ## Why the expiry is one write
//!
//! The store's [`Store::expire_lease`] does not just release an assignment. In
//! one transaction it releases the tenure as `LeaseExpired`, closes the session
//! that held the work as `Vanished`, records the agent as `Disconnected`, clears
//! its `current_task` view, and puts the task back to `todo` with a status
//! transition recording the move. The alternative — a release followed by
//! separate status and session writes — leaves a window in which the task reads
//! `in_progress` with no one holding it, which is exactly the orphaned state
//! MONITOR exists to report rather than to create. Folding them together makes
//! "an expired lease is a `todo` task with a retained tenure" a property of the
//! write.
//!
//! ## Liveness is derived, never stored
//!
//! [`Agent::liveness`] computes the lease question from `last_seen` on every
//! call. There is no `alive` flag to fall out of step and no cached verdict to
//! invalidate — which matters because the one thing a monitor must never do is
//! reclaim a lease from an agent whose heartbeat it has not checked.

use director_domain::agent::Liveness;
use director_domain::assignment::AgentAssignment;
use director_domain::ids::{AgentId, AssignmentId, ProjectId, SessionId, TaskId};
use director_domain::session::AgentSession;
use director_domain::task::{Task, TaskStatus};
use director_domain::{AgentRepository, AssignmentRepository, TaskRepository};
use director_store::Store;

use crate::MonitorError;

/// One round of MONITOR: what it found in flight, and what it did.
#[derive(Debug, Clone, Default)]
pub struct MonitorReport {
    /// Every task the round looked at, in the store's order. A quiet round is
    /// not an empty report — it is a report full of [`InFlight::Live`].
    pub in_flight: Vec<InFlight>,
}

impl MonitorReport {
    /// The leases the round reclaimed, in the order it reclaimed them.
    pub fn expired(&self) -> Vec<&ExpiredLease> {
        self.in_flight
            .iter()
            .filter_map(|entry| match entry {
                InFlight::Expired(lease) => Some(lease.as_ref()),
                _ => None,
            })
            .collect()
    }

    /// The tasks in flight with no tenant. MONITOR reports these and writes
    /// nothing; deciding what to do with work no one holds is REPLAN's job.
    pub fn orphaned(&self) -> Vec<&TaskId> {
        self.in_flight
            .iter()
            .filter_map(|entry| match entry {
                InFlight::Orphaned { task } => Some(task),
                _ => None,
            })
            .collect()
    }

    /// True if the round reclaimed nothing and found no anomalies. The state a
    /// polling loop wants to see: everything it started is still being worked.
    pub fn is_quiet(&self) -> bool {
        self.in_flight
            .iter()
            .all(|entry| matches!(entry, InFlight::Live { .. }))
    }
}

/// What the round found about one task ASSIGN started, and what it did.
#[derive(Debug, Clone)]
pub enum InFlight {
    /// The task is held, and the holder's heartbeat is fresh or merely stale.
    /// The lease stands; the round reports and moves on.
    Live {
        /// The task the round looked at.
        task: TaskId,
        /// The agent holding it.
        agent: AgentId,
        /// What the heartbeats said — `Working` or `Stale`. A holder past the
        /// stale window is [`InFlight::Expired`] instead: the round did
        /// something about it, and the report says what.
        liveness: Liveness,
    },
    /// The holder went past the stale window, and the round reclaimed the lease.
    ///
    /// Boxed because an expiry carries the full task, tenure, and session the
    /// round wrote, which dwarfs the other arms — an unboxed enum would size
    /// every `Live` report at the cost of a `MonitorReport` full of them.
    Expired(Box<ExpiredLease>),
    /// The task is `in_progress` and no active assignment holds it — the state
    /// a stranded handoff would leave behind. The round reports it and writes
    /// nothing: the store's atomic handoff is what keeps this rare, and
    /// deciding what to do with tenantless work is REPLAN's job, not MONITOR's.
    Orphaned {
        /// The task with no tenant.
        task: TaskId,
    },
}

/// A lease the round reclaimed, and everything the expiry wrote.
#[derive(Debug, Clone)]
pub struct ExpiredLease {
    /// The task whose lease ran out — now `todo` and handable again.
    pub task: Task,
    /// The tenure that ended, retained in the history as `lease_expired` rather
    /// than deleted.
    pub assignment: AgentAssignment,
    /// The session that held the work, closed as `vanished` if it was still
    /// live when the lease was reclaimed. `None` when the agent never
    /// acknowledged the handoff with a session — the lease expired before the
    /// work began.
    pub session: Option<AgentSession>,
}

/// Run the MONITOR step: survey the project's work in flight and reclaim the
/// leases that have run out.
///
/// A round is idempotent. A task whose lease it already expired is `todo`, so
/// the next round does not survey it; a task whose holder is quiet but not gone
/// is reported every round and written never. That is what makes it safe to run
/// on every loop tick.
pub async fn monitor(store: &Store, project_id: &ProjectId) -> Result<MonitorReport, MonitorError> {
    monitor_at(store, project_id, chrono::Utc::now()).await
}

/// [`monitor`], with the moment the round derives liveness at supplied by the
/// caller. Exposed so a test can pin the clock and move an agent from `Working`
/// to `Gone` without waiting an hour; production passes [`chrono::Utc::now`].
pub async fn monitor_at(
    store: &Store,
    project_id: &ProjectId,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<MonitorReport, MonitorError> {
    let tasks = store.tasks().list_tasks(project_id).await?;

    let mut in_flight = Vec::new();
    for task in tasks
        .iter()
        .filter(|task| task.status == TaskStatus::InProgress)
    {
        // The lease in force, if there is one. The assignment is the authority
        // on who holds a task, so it is what the round asks — not the task row,
        // which names no agent by design.
        let Some(assignment) = store
            .assignments()
            .active_assignment_for_task(&task.id)
            .await?
        else {
            in_flight.push(InFlight::Orphaned {
                task: task.id.clone(),
            });
            continue;
        };

        // The agent as it stands, and what its heartbeats say about the lease.
        // Liveness is derived here from `last_seen`, never read off a stored
        // flag, so the round's verdict is no staler than the heartbeat it
        // checked.
        let agent = store.agents().get_agent(&assignment.agent_id).await?;
        let liveness = agent.liveness(now);

        if !liveness.is_gone() {
            in_flight.push(InFlight::Live {
                task: task.id.clone(),
                agent: agent.id,
                liveness,
            });
            continue;
        }

        // The lease has run out. Everything from here is one transaction in the
        // store: the tenure is released, the session is closed, the agent is
        // recorded as gone, and the task goes back to `todo`.
        match store.expire_lease(&task.id, now).await? {
            Some((assignment, task, session)) => {
                in_flight.push(InFlight::Expired(Box::new(ExpiredLease {
                    task,
                    assignment,
                    session,
                })));
            }
            None => {
                // A concurrent writer ended the lease between this round's read
                // and its write. The task is in progress with no one holding it,
                // which is the orphan state — so it is reported the same way,
                // because that is what it is now.
                in_flight.push(InFlight::Orphaned {
                    task: task.id.clone(),
                });
            }
        }
    }

    Ok(MonitorReport { in_flight })
}

/// A request to record that an agent started the session doing its assignment.
///
/// The caller names the two ids — the tenure and the invocation — and Orqyn
/// derives the rest from the records it already holds, so the session the store
/// writes cannot disagree with the assignment it belongs to.
#[derive(Debug, Clone)]
pub struct AcknowledgeRequest {
    /// The assignment the agent was given. It must still be active.
    pub assignment_id: AssignmentId,
    /// The identifier the session will have. Each invocation is its own row, so
    /// an id is used once for the life of the project.
    pub session_id: SessionId,
}

/// The outcome of an acknowledgment: a tenure with a session behind it.
#[derive(Debug, Clone)]
pub struct Acknowledged {
    /// The assignment, now carrying the session doing its work.
    pub assignment: AgentAssignment,
    /// The session, recorded against the task and the agent.
    pub session: AgentSession,
}

/// Record that an agent started the work it was handed: the session is written
/// and attached to its assignment in one transaction.
///
/// Until this runs, an assignment is a handoff Orqyn recorded but no evidence
/// the work ever began. After it runs the tenure has an invocation behind it —
/// which is what a later lease expiry closes as `Vanished`, and what makes
/// "which session of which agent did this task" an ordinary query.
pub async fn acknowledge(
    store: &Store,
    request: AcknowledgeRequest,
) -> Result<Acknowledged, MonitorError> {
    acknowledge_at(store, request, chrono::Utc::now()).await
}

/// [`acknowledge`], with the start time supplied by the caller.
pub async fn acknowledge_at(
    store: &Store,
    request: AcknowledgeRequest,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Acknowledged, MonitorError> {
    // 1. The assignment the caller named. Loaded before validation rather than
    //    trusted from the request, because the question is about the world: the
    //    tenure's real status is what decides whether a session can be attached.
    let assignment = lookup(store, &request.assignment_id).await?;

    // 2. The task and the agent the tenure names. The session is built from
    //    these rather than from the request, so its project, task, agent, and
    //    machine cannot drift from the records the assignment points at.
    let task = store.tasks().get_task(&assignment.task_id).await?;
    let agent = store.agents().get_agent(&assignment.agent_id).await?;

    // 3. The legality check. Pure, once the loads have happened — the half of
    //    the step that can be tested without a store.
    validate_acknowledgment(&assignment)?;

    // 4. The session, derived entirely from records Orqyn holds. Its project,
    //    task, agent, and machine come from the records the assignment points
    //    at, so the session cannot disagree with the tenure it belongs to.
    let session = AgentSession::start(
        request.session_id.clone(),
        task.project_id.clone(),
        assignment.agent_id.clone(),
        agent.machine.clone(),
        Some(assignment.task_id.clone()),
    );

    // 5. The write, atomic: the session row and the link to the assignment land
    //    together, or neither does.
    let (assignment, session) = store
        .acknowledge_assignment(&request.assignment_id, &session, now)
        .await?;

    Ok(Acknowledged {
        assignment,
        session,
    })
}

/// The pure half of acknowledgment: given the assignment as it stands, can a
/// session be attached to it?
///
/// No store, no I/O, no clock — everything about the world has already been
/// loaded by [`acknowledge_at`]. Extracted so the rules can be tested directly.
pub fn validate_acknowledgment(assignment: &AgentAssignment) -> Result<(), MonitorError> {
    // A released tenure is over. Attaching a session to it would give an ended
    // assignment evidence of work it never did, and the store refuses the write
    // for the same reason — this check is what gives the caller a useful error
    // instead of a constraint violation.
    if !assignment.is_active() {
        return Err(MonitorError::AssignmentNotActive(assignment.id.clone()));
    }

    // A tenure with a session already has its invocation recorded. Two sessions
    // on one assignment is not a thing: the first one is the evidence trail, and
    // a second acknowledgment is a caller mistake rather than a new session.
    if assignment.session_id.is_some() {
        return Err(MonitorError::AlreadyAcknowledged {
            assignment: assignment.id.clone(),
            session: assignment.session_id.clone().expect("checked above"),
        });
    }

    Ok(())
}

/// Load an assignment, translating the store's "no such row" into the caller's
/// "you named an assignment Orqyn does not have" and leaving every other failure
/// a store failure.
async fn lookup(store: &Store, id: &AssignmentId) -> Result<AgentAssignment, MonitorError> {
    store
        .assignments()
        .get_assignment(id)
        .await
        .map_err(|err| match err {
            director_domain::StoreError::NotFound(_) => MonitorError::UnknownAssignment(id.clone()),
            other => MonitorError::Store(other.to_string()),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use director_domain::agent::{Agent, Harness};
    use director_domain::assignment::AssignmentStatus;
    use director_domain::ids::{AssignmentId, MachineId, SessionId, TaskId};

    /// An active tenure on TASK-42, with no session yet.
    fn active_assignment() -> AgentAssignment {
        AgentAssignment {
            id: AssignmentId::from_string("ASG-1"),
            task_id: TaskId::from_string("TASK-42"),
            agent_id: AgentId::from_string("AGENT-1"),
            session_id: None,
            status: AssignmentStatus::Active,
            assigned_at: chrono::Utc::now(),
            released_at: None,
            release_reason: None,
            note: None,
            state_version: 1,
        }
    }

    #[test]
    fn an_active_assignment_can_be_acknowledged() {
        assert!(validate_acknowledgment(&active_assignment()).is_ok());
    }

    #[test]
    fn a_released_assignment_cannot_acknowledge_a_session() {
        let mut assignment = active_assignment();
        assignment.release(director_domain::assignment::ReleaseReason::LeaseExpired);

        let err = validate_acknowledgment(&assignment).unwrap_err();
        assert!(
            matches!(err, MonitorError::AssignmentNotActive(ref id)
                if *id == AssignmentId::from_string("ASG-1")),
            "got {err:?}"
        );
    }

    #[test]
    fn an_assignment_with_a_session_cannot_acknowledge_another() {
        let mut assignment = active_assignment();
        assignment.activate(SessionId::from_string("SESS-1"));

        let err = validate_acknowledgment(&assignment).unwrap_err();
        assert!(
            matches!(err, MonitorError::AlreadyAcknowledged { ref assignment, ref session }
                if *assignment == AssignmentId::from_string("ASG-1")
                && *session == SessionId::from_string("SESS-1")),
            "got {err:?}"
        );
    }

    #[test]
    fn a_quiet_round_is_one_full_of_live_leases() {
        let quiet = MonitorReport {
            in_flight: vec![
                InFlight::Live {
                    task: TaskId::from_string("A"),
                    agent: AgentId::from_string("AGENT-1"),
                    liveness: Liveness::Working,
                },
                InFlight::Live {
                    task: TaskId::from_string("B"),
                    agent: AgentId::from_string("AGENT-2"),
                    liveness: Liveness::Stale,
                },
            ],
        };
        assert!(quiet.is_quiet());
        assert!(quiet.expired().is_empty());
        assert!(quiet.orphaned().is_empty());

        // A stale lease is still a live one: quiet, not gone.
        let mut noisy = quiet.clone();
        noisy.in_flight.push(InFlight::Orphaned {
            task: TaskId::from_string("C"),
        });
        assert!(!noisy.is_quiet());
        assert_eq!(noisy.orphaned().len(), 1);
    }

    #[test]
    fn liveness_derived_from_heartbeats_decides_the_rounds_outcome() {
        // The windows the round branches on, in the terms the domain speaks.
        let mut agent = Agent::register(
            AgentId::from_string("AGENT-1"),
            "claude-a",
            Harness::ClaudeCode,
            MachineId::from_string("MACH-a"),
            vec![],
        );
        let now = chrono::Utc::now();

        agent.last_seen = now;
        assert!(!agent.liveness(now).is_gone());

        agent.last_seen = now - chrono::Duration::minutes(45);
        assert!(!agent.liveness(now).is_gone());

        agent.last_seen = now - chrono::Duration::minutes(90);
        assert!(agent.liveness(now).is_gone());
    }
}
