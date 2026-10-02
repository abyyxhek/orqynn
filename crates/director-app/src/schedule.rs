//! The SCHEDULE step of Orqyn's loop.
//!
//! SCHEDULE is the step that drives the others. Every step before it
//! deliberately keeps its judgment outside the function — PLAN takes a stated
//! decomposition, ASSIGN takes a named agent, REPLAN takes a remediation per
//! task — because a judgment inside the function is a judgment a test cannot
//! supply and a caller cannot override. That left the loop able to run only
//! while somebody answers each question by hand. This step is the answer to
//! the one question that does not need a human: *which* of the ready tasks
//! goes to *which* of the available agents.
//!
//! ```text
//! OBSERVE → PLAN → SCHEDULE → MONITOR → VERIFY → REPLAN
//!                     ▲
//!                     │
//!             ready_tasks          ← what can be handed out
//!           + list_agents          ← who can take it
//!           + assignment_history   ← who has already tried
//! ```
//!
//! It composes [`crate::assign`], it does not bypass it. The round proposes a
//! pairing, and every proposal is applied through [`crate::assign::assign`],
//! which re-checks the handoff's legality against live state and does the
//! write. SCHEDULE owns the *choice*; ASSIGN still owns the *legality*. That
//! split is why this module writes nothing to the store itself and needs no
//! new store method — [`crate::assign::ready_tasks`], [`AgentRepository`]'s
//! `list_agents`, and [`AssignmentRepository`]'s `assignment_history` already
//! exist, and every write goes through a step that was already tested.
//!
//! ## The policy, and why it is these four rules
//!
//! [`match_agents`] is the whole policy, extracted as a pure function so it can
//! be tested without a store. Eligibility first, then a sort key:
//!
//! 1. **Eligible**: the agent's status says it can accept work, it is not
//!    already showing a current task, and it declares every capability the task
//!    requires. Both halves of the first two earn their keep:
//!    [`crate::assign::assign`] moves an agent's `current_task` but leaves its
//!    status `Available` — `Busy` arrives only when the agent acknowledges the
//!    work — so `current_task` is what excludes an agent that is already
//!    holding something, and `can_accept_work()` is what excludes one that has
//!    since gone quiet. Either alone would let a working agent through.
//! 2. **Fresh eyes first**: prefer an agent who has never held this task, per
//!    its assignment history. A task that is back to `todo` is a task a tenure
//!    ended without finishing, and handing it to the same agent again is the
//!    pairing most likely to repeat the outcome. In the common case this rule
//!    and rule 1 agree — a lease MONITOR expired leaves its holder
//!    `Disconnected`, which rule 1 already refuses — so the rule only actually
//!    bites for an agent that abandoned work while still alive. A prior holder
//!    is a **fallback**, never a prohibition: when the prior holder is the only
//!    eligible agent, forbidding it would strand the task forever, so the sort
//!    key de-prioritizes instead of excluding.
//! 3. **Specialist first**: among eligible agents, prefer the one declaring the
//!    *fewest* capabilities. A specialist should take the specialist task, so
//!    the generalist stays free for the task only a generalist can cover.
//!    Ordering the other way would spend the only agent that can do the later
//!    task on the one anybody could do.
//! 4. **Agent id** as the final tie-break, so the same state always yields the
//!    same pairing and a test is stable across runs.
//!
//! Task *priority* is deliberately not an axis. The plan's order is already the
//! priority order — [`crate::assign::ready_in_plan`] yields in it — and a
//! second priority system would disagree with the first.
//!
//! ## One task per agent per round
//!
//! An agent holds one task; that is the model, and the round honors it in two
//! places. The eligibility filter skips an agent showing a `current_task`, and
//! the round also carries the set of agents it has already committed *this
//! round*, because the store's view has not caught up with a proposal the round
//! itself just made. The second check is what makes two ready tasks competing
//! for one agent deterministic: the earlier task in plan order wins, and the
//! later one is reported as [`UnassignedReason::AgentTakenEarlier`] rather than
//! being handed to an agent the round knows is taken.
//!
//! ## Proposals are a snapshot; the write is live
//!
//! The round computes its pairings from a snapshot and applies them one at a
//! time through [`crate::assign::assign`], which re-validates against the live
//! store. A proposal computed a moment ago can be stale by the time it is
//! applied — another step moved the task, an agent went quiet, the plan was
//! superseded — and a stale proposal is *refused*, never an illegal write. That
//! is the difference from REPLAN, which validates every decision before
//! applying any: REPLAN's decisions can conflict with *each other*, so all must
//! be checked up front, while scheduler proposals can only conflict *through
//! the store*, and `assign` plus the store's own invariants already guard that.
//! Applying one at a time means the losing task is simply reported.
//!
//! A refused proposal is not an error, for the same reason a refused decision
//! is not one in REPLAN: it is a fact about the world the round read, not a
//! failure of the round. It lands in [`ScheduleReport::refused`] with the
//! reason, and nothing was written — the task is still `todo` and still
//! handable, so the next round can try again.
//!
//! ## What this step is not responsible for
//!
//! It does not invent work — that is PLAN's caller's judgment. It does not
//! create or supersede plans, judge work, or reclaim leases: those are PLAN,
//! VERIFY, and MONITOR. It touches only the tasks [`crate::assign::ready_tasks]
//! returns, so in-flight work is none of its business, and a task MONITOR
//! already reclaimed is back to `todo` with a holder who is `Disconnected` —
//! the two steps cannot overlap on the same task in the same tick.
//!
//! [AgentRepository]: director_domain::AgentRepository
//! [AssignmentRepository]: director_domain::AssignmentRepository

use std::collections::{HashMap, HashSet};

use director_domain::agent::Agent;
use director_domain::assignment::AgentAssignment;
use director_domain::ids::{AgentId, AssignmentId, IdGenerator, ProjectId, TaskId};
use director_domain::task::Task;
use director_domain::{AgentRepository, AssignmentRepository};

use director_store::Store;

use crate::assign::{self, AssignRequest};
use crate::{AssignError, ScheduleError};

/// Run the SCHEDULE step: hand every ready task in the active plan to the best
/// available agent.
///
/// Pairings are chosen by [`match_agents`], applied one at a time through
/// [`crate::assign::assign`], and every outcome — applied, refused, or left
/// without an agent — is reported. The round writes only through `assign`, so a
/// refused or unassigned task is exactly where the round found it.
///
/// `ids` mints the assignment ids, since this step is the one deciding who gets
/// what and there is no caller to name them.
pub async fn schedule(
    store: &Store,
    ids: &mut IdGenerator,
    project_id: &ProjectId,
) -> Result<ScheduleReport, ScheduleError> {
    schedule_at(store, ids, project_id, chrono::Utc::now()).await
}

/// [`schedule`], with the assignment time supplied by the caller. Exposed so a
/// test can pin the clock and distinguish two assignments; production passes
/// [`chrono::Utc::now`].
pub async fn schedule_at(
    store: &Store,
    ids: &mut IdGenerator,
    project_id: &ProjectId,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<ScheduleReport, ScheduleError> {
    // 1. What can be handed out right now, in the plan's execution order. This
    //    is the same read a caller would make to decide assignments by hand, so
    //    the scheduler and a human driver see the same work.
    let ready = assign::ready_tasks(store, project_id).await?;

    // 2. Who can take it. Every agent Orqyn knows; eligibility is a question
    //    for match_agents, not for the store.
    let agents = store.agents().list_agents().await?;

    let mut report = ScheduleReport::default();
    // The agents this round has already committed, which the store's
    // `current_task` view does not reflect yet. Without it, two ready tasks
    // would both be proposed to the same agent and the second would be refused
    // by `assign` instead of reported cleanly.
    let mut committed: HashSet<AgentId> = HashSet::new();

    for task in &ready {
        // 3. Who has already tried this task, so the policy can prefer an agent
        //    who has not. Read per task, because it is only the history *of this
        //    task* that matters.
        let history = store.assignments().assignment_history(&task.id).await?;
        let prior_tenures = count_tenures(&history);

        // 4. Rank the eligible agents, then take the best one not already taken
        //    by an earlier task in this round.
        let ranked = match_agents(task, &agents, &prior_tenures);
        let Some(agent_id) = ranked.into_iter().find(|id| !committed.contains(id)) else {
            // Nobody is left for this task. If any agent was eligible at all,
            // they were taken by an earlier task; otherwise nobody eligible
            // exists, which is a staffing signal worth surfacing on its own.
            report.unassigned.push(Unassigned {
                task_id: task.id.clone(),
                reason: unassigned_reason(&agents, task),
            });
            continue;
        };

        // 5. Apply the pairing through ASSIGN, which re-checks legality against
        //    the live store and does the write. A proposal is a snapshot; the
        //    write is live, and a stale proposal is refused rather than applied.
        let request = AssignRequest {
            project_id: project_id.clone(),
            task_id: task.id.clone(),
            agent_id: agent_id.clone(),
            assignment_id: ids.next::<AssignmentId>("sched"),
        };
        match assign::assign_at(store, request, now).await {
            Ok(assigned) => {
                committed.insert(agent_id.clone());
                report.assigned.push(Scheduled {
                    task_id: task.id.clone(),
                    agent_id,
                    assignment: assigned.assignment,
                });
            }
            // Nothing was written: the task is still `todo` and still handable,
            // so this is reported rather than raised. A refused pairing is a
            // fact about the state the round read, not a failure of the round.
            Err(reason) => report.refused.push(Refused {
                task_id: task.id.clone(),
                agent_id,
                reason,
            }),
        }
    }

    Ok(report)
}

/// How many tenures each agent has already had on a task, from its assignment
/// history. Zero for an agent who has never held it — the preferred pairing.
fn count_tenures(history: &[AgentAssignment]) -> HashMap<AgentId, usize> {
    let mut tenures = HashMap::new();
    for assignment in history {
        *tenures.entry(assignment.agent_id.clone()).or_insert(0) += 1;
    }
    tenures
}

/// Rank the agents who can take `task` right now, best first.
///
/// The policy in one place: an agent is eligible when its status can accept
/// work, it is not showing a current task, and it declares every capability the
/// task requires; eligible agents are ordered by fewest prior tenures on this
/// task, then by fewest declared capabilities, then by id. Pure — no store, no
/// I/O, no clock — so the whole policy is testable without a round.
pub fn match_agents(
    task: &Task,
    agents: &[Agent],
    prior_tenures: &HashMap<AgentId, usize>,
) -> Vec<AgentId> {
    // The eligibility filter. Both halves matter: `assign` moves an agent's
    // `current_task` but leaves its status `Available` until the agent
    // acknowledges, so `current_task` is what catches an agent already holding
    // work and the status is what catches one that has gone quiet.
    let mut eligible: Vec<&Agent> = agents
        .iter()
        .filter(|agent| agent.status.can_accept_work())
        .filter(|agent| agent.current_task.is_none())
        .filter(|agent| agent.has_all_capabilities(&task.required_capabilities))
        .collect();

    eligible.sort_by(|a, b| {
        prior_tenures
            .get(&a.id)
            .copied()
            .unwrap_or_default()
            .cmp(&prior_tenures.get(&b.id).copied().unwrap_or_default())
            .then_with(|| a.capabilities.len().cmp(&b.capabilities.len()))
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });

    eligible.into_iter().map(|agent| agent.id.clone()).collect()
}

/// Why a ready task was left without an agent, from the registry the round
/// already loaded.
fn unassigned_reason(agents: &[Agent], task: &Task) -> UnassignedReason {
    if agents.is_empty() {
        return UnassignedReason::NoAgentsAtAll;
    }
    let any_available = agents
        .iter()
        .any(|agent| agent.status.can_accept_work() && agent.current_task.is_none());
    if !any_available {
        UnassignedReason::NoAgentsAtAll
    } else if agents.iter().any(|agent| {
        agent.status.can_accept_work()
            && agent.current_task.is_none()
            && agent.has_all_capabilities(&task.required_capabilities)
    }) {
        // Somebody eligible exists, so the round must have committed them to an
        // earlier task. This is the report a caller uses to see contention.
        UnassignedReason::AgentTakenEarlier
    } else {
        UnassignedReason::NoEligibleAgent
    }
}

/// The outcome of a SCHEDULE round: the pairings it made, the ones the store
/// refused, and the work it could not pair at all.
///
/// No `PartialEq`: [`Refused`] carries an [`AssignError`], and equality on an
/// error enum would be equality on a failure vocabulary that is not this
/// step's to define. The round is compared by what it did, not by what went
/// wrong.
#[derive(Debug, Clone, Default)]
pub struct ScheduleReport {
    /// The pairings applied: a task, the agent now holding it, and the
    /// assignment record as the store wrote it.
    pub assigned: Vec<Scheduled>,
    /// Pairings the store refused. Nothing was written for any of these; each
    /// carries the [`AssignError`] that explains it.
    pub refused: Vec<Refused>,
    /// Ready work the round could not pair, with why. This is the report that
    /// makes the scheduler worth having over calling `assign` by hand: work
    /// that no available agent can do surfaces here instead of silently
    /// remaining `todo` round after round.
    pub unassigned: Vec<Unassigned>,
}

impl ScheduleReport {
    /// The ids of the tasks the round handed out.
    pub fn assigned(&self) -> impl Iterator<Item = &TaskId> {
        self.assigned.iter().map(|pairing| &pairing.task_id)
    }

    /// The tasks the store refused, with the agent and the reason.
    pub fn refused(&self) -> impl Iterator<Item = (&TaskId, &AgentId, &AssignError)> {
        self.refused
            .iter()
            .map(|refused| (&refused.task_id, &refused.agent_id, &refused.reason))
    }

    /// The ready work the round could not pair, with why.
    pub fn unassigned(&self) -> impl Iterator<Item = (&TaskId, UnassignedReason)> {
        self.unassigned
            .iter()
            .map(|unassigned| (&unassigned.task_id, unassigned.reason))
    }

    /// True if the round handed out every ready task it surveyed.
    ///
    /// A refused pairing is not settled — the task is still `todo` and still
    /// handable, so something is waiting. Work no available agent can do is not
    /// settled either, and it is the more important of the two: it will not
    /// clear on its own, and a caller should see it as a staffing question
    /// rather than a healthy tick.
    pub fn is_settled(&self) -> bool {
        self.refused.is_empty() && self.unassigned.is_empty()
    }
}

/// A pairing the round made and the store accepted.
#[derive(Debug, Clone)]
pub struct Scheduled {
    /// The task, now `in_progress`.
    pub task_id: TaskId,
    /// The agent now holding it.
    pub agent_id: AgentId,
    /// The assignment as the store wrote it, reloaded rather than echoed from
    /// the request.
    pub assignment: AgentAssignment,
}

/// A pairing the round proposed and the store refused.
#[derive(Debug, Clone)]
pub struct Refused {
    /// The task the round tried to hand out.
    pub task_id: TaskId,
    /// The agent the round chose for it.
    pub agent_id: AgentId,
    /// Why the store refused. Nothing was written.
    pub reason: AssignError,
}

/// Ready work the round could not pair with any agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unassigned {
    /// The task that stayed `todo`.
    pub task_id: TaskId,
    /// Why no pairing was made.
    pub reason: UnassignedReason,
}

/// Why a ready task was left without an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnassignedReason {
    /// Orqyn's registry is empty, so nothing can be handed out at all.
    NoAgentsAtAll,
    /// Agents are available, but none declares the capabilities the task
    /// requires. This is a staffing signal: the work is real and ready, and
    /// nobody on the registry can do it.
    NoEligibleAgent,
    /// An agent the task could have gone to was committed to an earlier task in
    /// the same round. Contention, not a shortage — the next round may find the
    /// agent free again.
    AgentTakenEarlier,
}

#[cfg(test)]
mod tests {
    use super::*;
    use director_domain::agent::{Agent, AgentStatus, Harness};
    use director_domain::capability::Capability;
    use director_domain::ids::{AgentId, MachineId, ProjectId, TaskId};
    use director_domain::task::Task;

    /// An agent named `id`, `Available`, with `caps` and no current task.
    fn agent(id: &str, caps: Vec<Capability>) -> Agent {
        let mut agent = Agent::register(
            AgentId::from_string(id),
            id,
            Harness::ClaudeCode,
            MachineId::from_string("MACH-a"),
            caps,
        );
        // `register` stamps `last_seen` at construction; a status the policy
        // reads is what the tests vary, so set it explicitly where needed.
        agent.status = AgentStatus::Available;
        agent
    }

    /// A task named `id` requiring `caps`.
    fn task(id: &str, caps: Vec<Capability>) -> Task {
        let mut task = Task::for_project(
            ProjectId::from_string("PROJ-1"),
            TaskId::from_string(id),
            id,
            "do it",
        );
        task.required_capabilities = caps;
        task
    }

    /// No prior tenures on anything: the fresh-eyes rule is inert.
    fn no_history() -> HashMap<AgentId, usize> {
        HashMap::new()
    }

    fn names(ranked: Vec<AgentId>) -> Vec<String> {
        ranked
            .into_iter()
            .map(|id| id.as_str().to_string())
            .collect()
    }

    #[test]
    fn an_agent_missing_a_required_capability_is_not_ranked() {
        let task = task("T", vec![Capability::Database]);
        let agents = vec![
            agent("AGENT-a", vec![Capability::Database]),
            agent("AGENT-b", vec![Capability::Frontend]),
        ];

        let ranked = match_agents(&task, &agents, &no_history());
        assert_eq!(names(ranked), vec!["AGENT-a"]);
    }

    #[test]
    fn an_unavailable_agent_is_not_ranked() {
        // Every status that cannot accept work is excluded, whatever its
        // capabilities.
        let task = task("T", vec![]);
        for status in [
            AgentStatus::Busy,
            AgentStatus::Stale,
            AgentStatus::Disconnected,
            AgentStatus::Offline,
        ] {
            let mut gone = agent("AGENT-a", vec![Capability::Coding]);
            gone.status = status;
            let free = agent("AGENT-b", vec![Capability::Coding]);

            let ranked = match_agents(&task, &[gone, free], &no_history());
            assert_eq!(names(ranked), vec!["AGENT-b"], "status {status:?}");
        }
    }

    #[test]
    fn an_agent_showing_a_current_task_is_not_ranked() {
        // `Available` is not enough: the denormalized view can lag the
        // assignment record, so an agent showing work is not free for more.
        let task = task("T", vec![]);
        let mut busy = agent("AGENT-a", vec![Capability::Coding]);
        busy.current_task = Some(TaskId::from_string("OTHER"));
        let free = agent("AGENT-b", vec![Capability::Coding]);

        let ranked = match_agents(&task, &[busy, free], &no_history());
        assert_eq!(names(ranked), vec!["AGENT-b"]);
    }

    #[test]
    fn a_human_requirement_is_only_satisfied_by_a_human_agent() {
        let task = task("T", vec![Capability::Human]);
        let machine = agent("AGENT-a", vec![Capability::Coding, Capability::Testing]);
        let mut human = agent("HUMAN-1", vec![Capability::Human]);
        human.harness = Harness::Human;

        // A generalist coding agent does not satisfy a `Human` requirement,
        // even though `Coding` is otherwise a super-capability.
        let ranked = match_agents(&task, &[machine, human], &no_history());
        assert_eq!(names(ranked), vec!["HUMAN-1"]);
    }

    #[test]
    fn a_specialist_is_preferred_over_a_generalist() {
        // The generalist can do either task; the specialist should get the
        // specialist task, leaving the generalist free for the rest. Ranking is
        // by how many capabilities an agent declares, so a generalist
        // declaring several ranks below a specialist declaring one.
        let task = task("T", vec![Capability::Database]);
        let generalist = agent(
            "AGENT-general",
            vec![
                Capability::Coding,
                Capability::Frontend,
                Capability::Backend,
            ],
        );
        let specialist = agent("AGENT-special", vec![Capability::Database]);

        let ranked = match_agents(&task, &[generalist, specialist], &no_history());
        assert_eq!(
            names(ranked),
            vec!["AGENT-special", "AGENT-general"],
            "the specialist ranks first"
        );
    }

    #[test]
    fn ties_break_on_agent_id_so_a_round_is_deterministic() {
        // Two identically-capable agents: the same state must always yield the
        // same pairing, so ids break the tie.
        let task = task("T", vec![Capability::Coding]);
        let b = agent("AGENT-b", vec![Capability::Coding]);
        let a = agent("AGENT-a", vec![Capability::Coding]);

        let ranked = match_agents(&task, &[b.clone(), a.clone()], &no_history());
        assert_eq!(names(ranked.clone()), vec!["AGENT-a", "AGENT-b"]);

        // And a second call on the same inputs yields the same answer.
        let again = match_agents(&task, &[b, a], &no_history());
        assert_eq!(names(ranked), names(again));
    }

    #[test]
    fn an_agent_who_never_held_the_task_ranks_above_one_who_did() {
        // A task back to `todo` is a task a tenure ended without finishing;
        // the agent who held it is the pairing most likely to repeat.
        let task = task("T", vec![Capability::Coding]);
        let first = agent("AGENT-first", vec![Capability::Coding]);
        let second = agent("AGENT-second", vec![Capability::Coding]);
        let mut prior = HashMap::new();
        prior.insert(AgentId::from_string("AGENT-first"), 1);

        let ranked = match_agents(&task, &[first.clone(), second.clone()], &prior);
        assert_eq!(
            names(ranked),
            vec!["AGENT-second", "AGENT-first"],
            "the agent who never held it ranks first"
        );

        // With no other candidate the prior holder is still returned — the rule
        // de-prioritizes, it does not strand the task.
        let alone = match_agents(&task, &[first], &prior);
        assert_eq!(names(alone), vec!["AGENT-first"]);
    }

    #[test]
    fn fewer_tenures_rank_above_more() {
        // Both agents have held the task; the one who held it less is the
        // better pairing.
        let task = task("T", vec![Capability::Coding]);
        let once = agent("AGENT-once", vec![Capability::Coding]);
        let thrice = agent("AGENT-thrice", vec![Capability::Coding]);
        let prior = [
            (AgentId::from_string("AGENT-once"), 1),
            (AgentId::from_string("AGENT-thrice"), 3),
        ]
        .into_iter()
        .collect();

        let ranked = match_agents(&task, &[once, thrice], &prior);
        assert_eq!(names(ranked), vec!["AGENT-once", "AGENT-thrice"]);
    }

    #[test]
    fn a_task_nobody_can_do_ranks_nobody() {
        let task = task("T", vec![Capability::Security]);
        let agents = vec![agent("AGENT-a", vec![Capability::Frontend])];

        assert!(match_agents(&task, &agents, &no_history()).is_empty());
    }

    #[test]
    fn no_agents_ranks_nobody() {
        let task = task("T", vec![]);
        assert!(match_agents(&task, &[], &no_history()).is_empty());
    }

    #[test]
    fn unassigned_reason_distinguishes_shortage_from_contention() {
        let task = task("T", vec![Capability::Database]);

        // An empty registry.
        assert_eq!(
            unassigned_reason(&[], &task),
            UnassignedReason::NoAgentsAtAll
        );

        // Agents exist, none available.
        let mut gone = agent("AGENT-a", vec![Capability::Database]);
        gone.status = AgentStatus::Offline;
        assert_eq!(
            unassigned_reason(&[gone], &task),
            UnassignedReason::NoAgentsAtAll
        );

        // Available, but nobody has the capability — the staffing signal.
        let mismatched = agent("AGENT-a", vec![Capability::Frontend]);
        assert_eq!(
            unassigned_reason(&[mismatched], &task),
            UnassignedReason::NoEligibleAgent
        );

        // Somebody eligible exists, so they must have been taken earlier.
        let eligible = agent("AGENT-a", vec![Capability::Database]);
        assert_eq!(
            unassigned_reason(&[eligible], &task),
            UnassignedReason::AgentTakenEarlier
        );
    }

    #[test]
    fn a_round_is_settled_only_when_nothing_is_left_waiting() {
        // Nothing handed out and nothing waiting: an empty round of an empty
        // plan.
        assert!(ScheduleReport::default().is_settled());

        // A handed-out task alone is settled — the round did its work.
        let mut done = ScheduleReport::default();
        done.assigned.push(Scheduled {
            task_id: TaskId::from_string("T"),
            agent_id: AgentId::from_string("AGENT-a"),
            assignment: AgentAssignment::propose(
                AssignmentId::from_string("ASG-1"),
                TaskId::from_string("T"),
                AgentId::from_string("AGENT-a"),
            ),
        });
        assert!(done.is_settled());

        // A refused pairing is not settled: the task is still handable.
        let mut refused = done.clone();
        refused.refused.push(Refused {
            task_id: TaskId::from_string("T2"),
            agent_id: AgentId::from_string("AGENT-b"),
            reason: AssignError::NoActivePlan(ProjectId::from_string("PROJ-1")),
        });
        assert!(!refused.is_settled());

        // Work with no agent is not settled either, and it will not clear on
        // its own.
        let mut stranded = done.clone();
        stranded.unassigned.push(Unassigned {
            task_id: TaskId::from_string("T3"),
            reason: UnassignedReason::NoEligibleAgent,
        });
        assert!(!stranded.is_settled());
    }
}
