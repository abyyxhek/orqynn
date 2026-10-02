//! The REPLAN step of Orqyn's loop.
//!
//! REPLAN reads what the loop's own verdicts left behind. VERIFY can judge a
//! task `failed`; MONITOR can leave a task `in_progress` with no one holding
//! it; a dependency that reached a dead end breaks the premise of everything
//! depending on it. None of those states is work being done, and none of them
//! is reachable from `done` — the loop produced them, and the loop has to
//! answer for them. REPLAN is that answer.
//!
//! ```text
//! OBSERVE → PLAN → ASSIGN → MONITOR → VERIFY → REPLAN
//!                                            ▲
//!                                            │
//!                                   failed tasks
//!                                   orphaned tasks
//!                                   + a caller's decision
//!                                     for each
//! ```
//!
//! ## Why this step exists
//!
//! Every other step exists to move work *forward*. REPLAN exists because moving
//! work forward is not always the right thing to do, and the loop's earlier
//! steps deliberately stop short of deciding that. MONITOR puts an expired lease
//! back to `todo` and says the rest is REPLAN's job; VERIFY marks a task
//! `failed` and says the same. Those are IOUs, and this step is where they are
//! paid: without it, a failed task stays failed forever and nothing in the
//! system can say otherwise.
//!
//! ## What REPLAN is responsible for, and what it is not
//!
//! REPLAN owns the *response*. It surveys a project's tasks, finds every one
//! whose state is a verdict the loop itself produced, and applies a decision
//! the caller supplies for each. Three remediations exist, and only three:
//!
//! - [`Remediation::Retry`] — the task goes back to `todo` and is handable
//!   again. The work is still worth doing; the last attempt simply did not
//!   complete it.
//! - [`Remediation::Rework`] — back to `todo`, with an amended objective and
//!   expected outputs. This is the remediation for "the check failed because
//!   the criterion was wrong", which is a real condition and not the same as
//!   retrying against an unchanged one.
//! - [`Remediation::Cancel`] — terminal `cancelled`, with the reason recorded
//!   as a [DecisionRecord]. The work is no longer worth doing, and the record
//!   of *why* it was abandoned outlives the task.
//!
//! REPLAN does **not** decide *which* remediation applies. That judgment
//! arrives from the caller, exactly as PLAN's decomposition arrives from the
//! caller as `TaskSpec`s, because "this failure means the criterion was wrong,
//! not the implementation" is a reasoning step. Keeping it outside the function
//! is what keeps the function deterministic and testable; REPLAN's job is to
//! make a stated decision durable and consistent, not to have the decision.
//!
//! It also does not create or supersede a plan, does not assign agents, and
//! does not judge work. Writing a whole new plan is PLAN's job — a caller that
//! has decided the decomposition itself is wrong calls [`crate::plan`] with a
//! new plan id, which supersedes the sitting one. REPLAN is for the surgical
//! case: one task's verdict needs an answer, and the rest of the plan still
//! holds.
//!
//! ## Three conditions, and only three
//!
//! [`Condition`] is deliberately exhaustive over the states that need a
//! caller's decision, and deliberately exclusive of everything else:
//!
//! - A `failed` task is a verdict VERIFY reached, and it needs an answer.
//! - An orphaned task — `in_progress` with no active assignment — is the state
//!   MONITOR reports and writes nothing about. It is not a failure of the work;
//!   it is a failure of the handoff, and the question is the same: what now?
//! - A task with a dependency that reached a dead end has a broken premise. Its
//!   own status may be a perfectly healthy `todo`; the problem is that it can
//!   never become ready, because [`Task::is_ready_given`] requires every
//!   dependency to be `Done`. The task is not broken, its situation is.
//!
//! Everything else is deliberately out of scope. A `verification_pending` task
//! is still being judged — VERIFY owns it, and REPLAN second-guessing a pending
//! verdict would race the step producing it. A `blocked` task is waiting on a
//! [Blocker], and blockers are their own entity with their own lifecycle; a
//! `done` task is finished and needs nothing.
//!
//! ## Why the cascade is reported, not written
//!
//! Cancelling a task strands its dependents: their premise breaks the moment
//! the dependency does. REPLAN reports every stranded dependent in the round's
//! result and writes nothing about any of them. That is safe by construction,
//! not by trust — [`crate::assign::ready_in_plan`] applies `is_ready_given`
//! over the current task statuses before handing anything out, so a task whose
//! dependency is not `Done` is never assigned no matter how ready its own row
//! looks. Reporting is enough: the caller learns exactly which tasks a
//! cancellation stranded, and decides each one next round.
//!
//! Writing them — to `blocked`, with a persisted [Blocker] — would be the
//! alternative, and it is the wrong one for this step. It would make REPLAN
//! own a second entity's lifecycle, and it would record a conclusion the caller
//! has not reached yet. A stranded dependent is a question, and REPLAN's job is
//! to put the question where someone can see it, not to answer it.
//!
//! ## A refused round writes nothing
//!
//! Every decision is validated against the survey *before* any of them is
//! applied, so a round that rejects one decision applies none of them: a retry
//! cannot be the response to a broken premise, a decision naming a task Orqyn
//! has no record of is an error rather than a silent skip, and a task the
//! caller decided twice is refused rather than resolved by the round. All or
//! nothing is what keeps a partially applied round — half the caller's intent
//! durable, the other half lost — from being a state the store can hold.
//!
//! ## Idempotency
//!
//! The round is safe to run on every tick. A task it retried is `todo`, a task
//! it cancelled is `cancelled`, and neither is any of the three conditions, so
//! the next round does not survey them. A stranded dependent *is* surveyed
//! again — it is premise-broken, and it stays premise-broken until the caller
//! answers for it — which is safe precisely because the round wrote nothing
//! about it.
//!
//! [DecisionRecord]: director_domain::decision::Decision
//! [Blocker]: director_domain::blocker::Blocker
//! [`Task::is_ready_given`]: director_domain::task::Task::is_ready_given

use std::collections::{HashMap, HashSet};

use director_domain::decision::Decision as DecisionRecord;
use director_domain::ids::{AgentId, DecisionId, ProjectId, TaskId};
use director_domain::task::{ExpectedOutput, Task, TaskStatus};
use director_domain::{AssignmentRepository, TaskRepository};
use director_store::Store;

use crate::ReplanError;

/// A request to decide what happens to the tasks the loop stopped on.
///
/// The caller names a project and a decision for each task it has an answer
/// for. Tasks the survey finds that the caller does not decide are reported and
/// left as they are — the round is not obligated to answer every question, and
/// a partial round is a legal, useful state.
#[derive(Debug, Clone)]
pub struct ReplanRequest {
    /// The project whose tasks are being decided. Must be known to Orqyn.
    pub project_id: ProjectId,
    /// One decision per task the caller is answering. A task may appear at most
    /// once; a second entry for the same task is rejected rather than applied
    /// twice.
    pub decisions: Vec<TaskDecision>,
    /// The agent authorizing these decisions, recorded on every [DecisionRecord]
    /// a cancellation writes. Provenance only: this does not assign the agent to
    /// anything, and is `None` when the loop is deciding on its own authority.
    ///
    /// [DecisionRecord]: director_domain::decision::Decision
    pub decided_by: Option<AgentId>,
}

/// One task the caller has decided the fate of.
#[derive(Debug, Clone)]
pub struct TaskDecision {
    /// The task this decision is about.
    pub task_id: TaskId,
    /// What to do with it.
    pub remediation: Remediation,
    /// Why, for a cancellation. Recorded as a [DecisionRecord] so the reason a
    /// task was abandoned stays answerable after the task is terminal. Ignored
    /// for the other remediations, which carry their own record: the task's own
    /// history row.
    ///
    /// [DecisionRecord]: director_domain::decision::Decision
    pub reason: Option<String>,
}

/// What to do with a task the loop stopped on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remediation {
    /// Put the task back to `todo`, handable again. The work is still worth
    /// doing; the last attempt did not complete it.
    Retry,
    /// Put the task back to `todo` with an amended contract. This is the
    /// remediation for a failure caused by the *criterion* being wrong rather
    /// than the implementation — a check that failed because it tested the
    /// wrong thing is not fixed by running it again.
    Rework {
        /// A new objective, replacing the task's current one. `None` leaves the
        /// objective alone.
        objective: Option<String>,
        /// New expected outputs, replacing the task's current ones. `None`
        /// leaves them alone. Amending the outputs is how a caller fixes a
        /// criterion that was wrong.
        expected_outputs: Option<Vec<ExpectedOutput>>,
    },
    /// Mark the task `cancelled`, terminal, with the reason recorded. The work
    /// is no longer worth doing.
    Cancel,
}

/// What the round found wrong with a task, and by implication what kind of
/// answer it needs.
///
/// Exhaustive by design: a task the loop stopped on is exactly one of these
/// three, and a task that is none of them does not need REPLAN's attention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    /// VERIFY judged the work and it did not pass. The task is `failed`.
    Failed,
    /// The task is `in_progress` and no active assignment holds it — the state
    /// MONITOR reports and leaves standing. The handoff, not the work, is what
    /// broke.
    Orphaned,
    /// A dependency reached a dead end, so this task can never become ready.
    /// The task itself may be a perfectly healthy `todo`; its situation is what
    /// is broken.
    PremiseBroken {
        /// The dependency that is not going to be `Done`.
        dependency: TaskId,
        /// The dead-end status it reached.
        dependency_status: TaskStatus,
    },
}

/// One round of REPLAN: every task needing a decision, and what the round did
/// about it.
#[derive(Debug, Clone, Default)]
pub struct ReplanReport {
    /// Every task the round surveyed, in the store's order. A round that found
    /// nothing needing a decision is an empty report, not an error — nothing is
    /// stopped, and there is nothing to answer.
    pub surveyed: Vec<Surveyed>,
}

impl ReplanReport {
    /// The tasks the round put back to `todo` — failed or orphaned work the
    /// caller decided is still worth doing, now handable again.
    pub fn retried(&self) -> impl Iterator<Item = &TaskId> {
        self.surveyed
            .iter()
            .filter_map(|entry| match &entry.outcome {
                Outcome::Applied {
                    remediation: Remediation::Retry | Remediation::Rework { .. },
                    ..
                } => Some(&entry.task),
                _ => None,
            })
    }

    /// The tasks the round cancelled — terminal now, and no longer the loop's
    /// concern.
    pub fn cancelled(&self) -> impl Iterator<Item = &TaskId> {
        self.surveyed
            .iter()
            .filter_map(|entry| match &entry.outcome {
                Outcome::Applied {
                    remediation: Remediation::Cancel,
                    ..
                } => Some(&entry.task),
                _ => None,
            })
    }

    /// The tasks the round refused to decide, with why. A refused round wrote
    /// nothing, so every one of these is exactly where the round found it.
    pub fn refused(&self) -> impl Iterator<Item = (&TaskId, &RemediationError)> {
        self.surveyed
            .iter()
            .filter_map(|entry| match &entry.outcome {
                Outcome::Refused { error } => Some((&entry.task, error)),
                _ => None,
            })
    }

    /// The tasks the round found needing a decision and the caller did not
    /// answer. Includes every dependent a cancellation stranded: they are here
    /// because they need an answer, and they are still waiting for one.
    pub fn undecided(&self) -> impl Iterator<Item = (&TaskId, &Condition)> {
        self.surveyed
            .iter()
            .filter_map(|entry| match (&entry.condition, &entry.outcome) {
                (Some(condition), Outcome::NotDecided) => Some((&entry.task, condition)),
                _ => None,
            })
    }

    /// True if the round left nothing waiting on an answer. The state a polling
    /// loop wants to see: nothing is stopped, and nothing stopped is still open.
    ///
    /// A task the caller named that turned out to be healthy counts as settled:
    /// it needed no decision, and none was written. So does a task whose
    /// condition disappeared over the course of the round — the question it was
    /// reported under is no longer being asked. A task the caller did not
    /// answer, or a decision the round refused, does not — both leave an open
    /// question the caller has to come back to.
    pub fn is_settled(&self) -> bool {
        self.surveyed.iter().all(|entry| {
            matches!(
                entry.outcome,
                Outcome::Applied { .. } | Outcome::NothingNeeded
            )
        })
    }
}

/// What the round concluded about one surveyed task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Surveyed {
    /// The task.
    pub task: TaskId,
    /// What the round found wrong with it. `None` when the survey found nothing
    /// — either the task is healthy, or the caller named a task Orqyn has no
    /// record of, so there was no task to classify.
    pub condition: Option<Condition>,
    /// What the round did, and why.
    pub outcome: Outcome,
}

/// What a round did about one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The caller decided this task and the round applied it. The task's new
    /// status is what the remediation maps to.
    Applied {
        /// The remediation that was applied.
        remediation: Remediation,
        /// The status the task now holds.
        status: TaskStatus,
    },
    /// The caller decided this task and the round refused. Nothing was written
    /// about it.
    Refused {
        /// Why the round would not apply the decision.
        error: RemediationError,
    },
    /// The caller did not decide this task. It is reported, and left exactly as
    /// the round found it.
    NotDecided,
    /// The caller named a task the survey found healthy. There was nothing to
    /// decide, and nothing was written. This is also the outcome the post-apply
    /// re-survey returns a task to when the condition it was reported under has
    /// disappeared — a premise the round itself repaired, for instance — so an
    /// entry never carries `NotDecided` without a condition to answer.
    NothingNeeded,
}

/// Why a round refused a decision the caller asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemediationError {
    /// The caller asked for a rework that amends nothing — neither the objective
    /// nor the expected outputs. A rework with no amendment is a retry wearing a
    /// different name, and a retry that reaches [`validate_decision`] has already
    /// passed its own check, so this is the only way a decision can be empty.
    ///
    /// Note what this is *not*: it is not a claim that the task needs no
    /// decision. The task is in a condition — that is the only way it reached
    /// [`validate_decision`] — and it still needs an answer, just a real one. A
    /// task that is genuinely healthy is never refused; it is reported as
    /// [`Outcome::NothingNeeded`] instead, because a caller's unnecessary
    /// request is not a failure of the round.
    NoOpRework,
    /// The caller asked to retry a task whose premise is broken. Retrying it
    /// would put it back to `todo`, where `ready_in_plan` would still refuse to
    /// hand it out because its dependency is not `Done` — the task would look
    /// handable in the store and never be assigned, which is worse than an
    /// honest refusal.
    RetryOnBrokenPremise {
        /// The dependency that is not going to become `Done`.
        dependency: TaskId,
    },
    /// The caller named a task Orqyn has no record of. An unknown task cannot
    /// have a verdict to respond to, so this is an error rather than a skip — a
    /// silent skip would let a caller's typo pass as a decision that landed.
    UnknownTask,
    /// The caller decided the same task twice. A task gets one remediation per
    /// round; which of two conflicting ones to apply is not a question the
    /// round should resolve.
    DuplicateDecision,
}

/// Run the REPLAN step: survey the tasks the loop stopped on, validate the
/// caller's decisions against the survey, and apply them.
///
/// Every decision is validated before any is applied, so a round that rejects
/// one decision writes nothing at all. Tasks the survey finds that the caller
/// does not decide are reported and left alone.
pub async fn replan(store: &Store, request: ReplanRequest) -> Result<ReplanReport, ReplanError> {
    replan_at(store, request, chrono::Utc::now()).await
}

/// [`replan`], with the moment the round records supplied by the caller.
pub async fn replan_at(
    store: &Store,
    request: ReplanRequest,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<ReplanReport, ReplanError> {
    let tasks = store.tasks().list_tasks(&request.project_id).await?;

    // A task is orphaned only if it is in flight with no active assignment, so
    // the survey needs the assignment layer, not just the task rows. Loaded
    // once, up front, so every surveyed task is classified against the same
    // picture of who holds what.
    let mut held = HashMap::new();
    for task in &tasks {
        held.insert(
            task.id.clone(),
            store
                .assignments()
                .active_assignment_for_task(&task.id)
                .await?
                .is_some(),
        );
    }

    // The pure half: what is wrong with each task, from the task list and the
    // tenure map alone. Extracted so the classification rules are testable
    // without a store.
    let conditions = survey(&tasks, &held);

    // Index the caller's decisions by task. A task appearing twice is refused
    // as a duplicate — including its first entry, because applying one of two
    // conflicting decisions would be silently resolving the caller's
    // contradiction.
    let mut counts = HashMap::new();
    for decision in &request.decisions {
        *counts.entry(&decision.task_id).or_insert(0usize) += 1;
    }
    let duplicates: HashSet<&TaskId> = counts
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(task_id, _)| *task_id)
        .collect();
    let decided: HashMap<&TaskId, &TaskDecision> = request
        .decisions
        .iter()
        .map(|decision| (&decision.task_id, decision))
        .collect();

    let mut surveyed = Vec::with_capacity(tasks.len());
    for task in &tasks {
        let condition = conditions.get(&task.id).cloned();

        // A task the caller named that the survey found healthy. Reported so the
        // caller knows the decision was not applied, rather than vanishing.
        if condition.is_none() {
            if decided.contains_key(&task.id) && !duplicates.contains(&task.id) {
                surveyed.push(Surveyed {
                    task: task.id.clone(),
                    condition: None,
                    outcome: Outcome::NothingNeeded,
                });
            }
            continue;
        }

        let outcome = if duplicates.contains(&task.id) {
            Outcome::Refused {
                error: RemediationError::DuplicateDecision,
            }
        } else {
            match decided.get(&task.id) {
                None => Outcome::NotDecided,
                Some(decision) => {
                    match validate_decision(task, condition.as_ref().unwrap(), decision) {
                        Ok(()) => Outcome::Applied {
                            remediation: decision.remediation.clone(),
                            status: remediation_destination(&decision.remediation),
                        },
                        Err(error) => Outcome::Refused { error },
                    }
                }
            }
        };

        surveyed.push(Surveyed {
            task: task.id.clone(),
            condition,
            outcome,
        });
    }

    // A decision naming a task Orqyn has no record of. These cannot be
    // classified — there is no task to read a status from — so they carry no
    // condition, only the refusal.
    for task_id in decided.keys() {
        if !tasks.iter().any(|task| &task.id == *task_id) {
            surveyed.push(Surveyed {
                task: (*task_id).clone(),
                condition: None,
                outcome: Outcome::Refused {
                    error: RemediationError::UnknownTask,
                },
            });
        }
    }

    // Nothing is applied until every decision has been validated. A round with
    // a refusal stops here, leaving the store exactly as it found it.
    if surveyed
        .iter()
        .any(|entry| matches!(entry.outcome, Outcome::Refused { .. }))
    {
        return Ok(ReplanReport { surveyed });
    }

    // Apply. Each decided task is one task write — status, amended contract, and
    // the history transition the store records with it — and a cancellation is
    // one *composite* write: the task and the decision that explains it land in
    // a single transaction, so the store can never hold a cancelled task whose
    // reason is missing.
    for entry in &mut surveyed {
        let Outcome::Applied {
            remediation,
            status,
        } = &entry.outcome
        else {
            continue;
        };
        let Some(decision) = decided.get(&entry.task) else {
            continue;
        };

        // Reloaded rather than reused from the survey list, because
        // `update_task` is an optimistic-concurrency write keyed on the version
        // the caller read, and the version the round holds is the one it needs
        // to present back.
        let mut task = store.tasks().get_task(&entry.task).await?;
        task.status = *status;
        if let Remediation::Rework {
            objective,
            expected_outputs,
        } = remediation
        {
            if let Some(objective) = objective {
                task.objective = objective.clone();
            }
            if let Some(expected_outputs) = expected_outputs {
                task.expected_outputs = expected_outputs.clone();
            }
        }
        task.touch();

        if let Remediation::Cancel = remediation {
            // One store operation, not two. The task's cancellation and the
            // decision that records why are a single transaction, so the round
            // can never leave a task terminal and unexplained — the reason a
            // task was abandoned is what makes the cancellation meaningful, and
            // a cancellation the store cannot explain is not a decision that
            // landed.
            let id = DecisionId::from_string(format!("DEC-CANCEL-{}", entry.task.as_str()));
            let mut record = DecisionRecord::new(
                id,
                format!("cancel task {}", entry.task.as_str()),
                decision
                    .reason
                    .clone()
                    .unwrap_or_else(|| "no reason recorded".to_string()),
            )
            .in_task(entry.task.clone());
            record.made_at = now;
            record.updated_at = now;
            if let Some(agent) = &request.decided_by {
                record = record.by(agent.clone());
            }
            store.cancel_task(&task, &record, now).await?;
        } else {
            store.tasks().update_task(&task).await?;
        }
    }

    // The round's own writes can change the survey. Cancelling a task strands
    // its dependents — and a dependent already surveyed against the dependency's
    // *old* status is now stranded for a different reason. So the survey runs
    // again over the world the round produced, and every entry the round neither
    // decided nor refused is re-read against it: an undecided task takes its
    // current condition, and a task the round stranded for the first time is
    // appended as undecided. Nothing is written for any of them; the report is
    // where the caller learns it owes them an answer.
    //
    // A task the caller named that was healthy at survey time — `NothingNeeded`
    // — is re-read too, and it is the case to be careful with: the round can
    // strand it by cancelling its dependency, which means the caller's decision
    // for it is no longer the answer to the question it is now asking. Leaving
    // the outcome alone would have `is_settled` call the round settled over a
    // task that needs an answer, and `undecided` skip it entirely — the report
    // would hide the very thing the re-survey exists to find. So it becomes
    // `NotDecided`: the caller's request was not applied, and the task's new
    // condition is what it owes an answer to.
    //
    // The mirror case is why this loop reads both outcomes and both branches:
    // an undecided task whose condition *disappeared* — the round retried the
    // dependency it was broken on, so the premise the question was about is
    // intact again. Leaving it `NotDecided` with no condition would be just as
    // self-contradicting a report in the other direction: `undecided` would
    // skip it because there is nothing to answer, while `is_settled` refused to
    // call the round settled. So it goes back to `NothingNeeded`, which is what
    // both accessors say about a task with no condition, and the report stays
    // consistent in both directions the survey can move.
    let after = store.tasks().list_tasks(&request.project_id).await?;
    let conditions_after = survey(&after, &held);
    for entry in &mut surveyed {
        if !matches!(entry.outcome, Outcome::NotDecided | Outcome::NothingNeeded) {
            continue;
        }
        entry.condition = conditions_after.get(&entry.task).cloned();
        match entry.condition {
            // The condition the task was reported under is gone, so the question
            // is gone with it. Nothing was written about the task either way.
            None => entry.outcome = Outcome::NothingNeeded,
            // The round's own writes stranded a task the caller had named as
            // healthy, so the caller's decision is no longer an answer to the
            // question the task is asking now.
            Some(_) if matches!(entry.outcome, Outcome::NothingNeeded) => {
                entry.outcome = Outcome::NotDecided;
            }
            // Still stopped, and still waiting on the same kind of answer.
            Some(_) => {}
        }
    }
    for task in &after {
        if surveyed.iter().any(|entry| entry.task == task.id) {
            continue;
        }
        if let Some(condition) = conditions_after.get(&task.id) {
            surveyed.push(Surveyed {
                task: task.id.clone(),
                condition: Some(condition.clone()),
                outcome: Outcome::NotDecided,
            });
        }
    }

    Ok(ReplanReport { surveyed })
}

/// The status a remediation moves a task to.
///
/// Kept beside [`validate_decision`] so the mapping from a remediation to a
/// write is visible in one place. Both remediations that put work back in
/// flight land on `todo` — the difference between them is what the task's
/// *contract* looks like afterward, not its status.
fn remediation_destination(remediation: &Remediation) -> TaskStatus {
    match remediation {
        Remediation::Retry | Remediation::Rework { .. } => TaskStatus::Todo,
        Remediation::Cancel => TaskStatus::Cancelled,
    }
}

/// The pure half of a decision: given the condition the survey found and the
/// decision the caller made, may the round apply it?
///
/// The task as it stands is accepted too, because the rules are stated over it
/// — "a retry of a task whose dependency is a dead end" is a rule about the
/// task's situation — but the two rules in force read only the condition and the
/// decision, so the task is unused until a rule needs the task's own state. No
/// store, no I/O, no clock: everything about the world has already been gathered
/// by [`replan_at`]. Extracted so the rules can be tested directly, because the
/// order of precedence between them is the whole design.
pub fn validate_decision(
    _task: &Task,
    condition: &Condition,
    decision: &TaskDecision,
) -> Result<(), RemediationError> {
    // A retry of a task whose premise is broken would move it to `todo`, where
    // it would look handable and never be handed out. The caller has to answer
    // the broken premise — by reworking the task so it no longer depends on the
    // dead end, or by cancelling it — rather than asking for a retry that
    // cannot produce work.
    if let Condition::PremiseBroken { dependency, .. } = condition {
        if let Remediation::Retry = decision.remediation {
            return Err(RemediationError::RetryOnBrokenPremise {
                dependency: dependency.clone(),
            });
        }
    }

    // A rework that changes nothing is a retry in disguise. (An *empty*
    // amendment — `Some(vec![])` — clears the expected outputs, which is a real
    // change, so it is allowed. The caller meant it.)
    if let Remediation::Rework {
        objective,
        expected_outputs,
    } = &decision.remediation
    {
        if objective.is_none() && expected_outputs.is_none() {
            return Err(RemediationError::NoOpRework);
        }
    }

    Ok(())
}

/// Classify every task: which of the three conditions, if any, does it have?
///
/// Pure over the task list and the tenure map, so the classification rules are
/// testable without a store. Returns a map rather than a vec so [`replan_at`]
/// can look a task up by id without a linear search per decision.
///
/// A task is an orphan only if it is in flight *and* nothing holds it; a task
/// being worked by an agent sitting on it is not stopped, it is just slow, and
/// MONITOR's liveness windows are what decide whether that is a problem. A
/// premise is broken by a dependency that reached a dead end — `failed` or
/// `cancelled` — because those are the terminal statuses that are not `Done`,
/// and only `Done` satisfies `is_ready_given`.
pub fn survey(tasks: &[Task], held: &HashMap<TaskId, bool>) -> HashMap<TaskId, Condition> {
    let status_of = |id: &TaskId| {
        tasks
            .iter()
            .find(|task| &task.id == id)
            .map(|task| task.status)
    };

    let mut conditions = HashMap::new();
    for task in tasks {
        if let Some(condition) = condition_for(task, held) {
            conditions.insert(task.id.clone(), condition);
        }
    }

    // A premise broken by a dead-end dependency, evaluated for every task the
    // first pass did not classify. A failed task with a failed dependency stays
    // `Failed` — its own verdict is the more actionable one — which is why this
    // runs second and skips anything already classified.
    for task in tasks {
        if conditions.contains_key(&task.id) {
            continue;
        }
        for dependency in &task.dependencies {
            let Some(dependency_status) = status_of(dependency) else {
                continue;
            };
            if is_dead_end(dependency_status) {
                conditions.insert(
                    task.id.clone(),
                    Condition::PremiseBroken {
                        dependency: dependency.clone(),
                        dependency_status,
                    },
                );
                break;
            }
        }
    }

    conditions
}

/// The condition a single task has, or `None` if it is healthy.
///
/// Extracted from [`survey`] so the caller-named-task paths in [`replan_at`] —
/// a task the survey found nothing wrong with, or one Orqyn has no record of —
/// can classify without reconstructing the whole map.
fn condition_for(task: &Task, held: &HashMap<TaskId, bool>) -> Option<Condition> {
    match task.status {
        TaskStatus::Failed => Some(Condition::Failed),
        TaskStatus::InProgress if !held.get(&task.id).copied().unwrap_or_default() => {
            Some(Condition::Orphaned)
        }
        _ => None,
    }
}

/// True if a task in this status is a dead end for anything depending on it.
/// `Done` satisfies a dependency; `Failed` and `Cancelled` never will, and
/// those are the only two terminal statuses that are not `Done`.
fn is_dead_end(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Failed | TaskStatus::Cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use director_domain::ids::TaskId;

    /// A task in the given status, with the given dependencies.
    fn task(id: &str, status: TaskStatus, dependencies: Vec<TaskId>) -> Task {
        let mut task = Task::for_project(
            ProjectId::from_string("PROJ-1"),
            TaskId::from_string(id),
            id,
            id,
        );
        task.status = status;
        task.dependencies = dependencies;
        task
    }

    /// Nothing holds any of these tasks.
    fn unheld(ids: &[&str]) -> HashMap<TaskId, bool> {
        ids.iter()
            .map(|id| (TaskId::from_string(*id), false))
            .collect()
    }

    #[test]
    fn a_failed_task_is_surveyed_as_failed() {
        let tasks = vec![task("A", TaskStatus::Failed, vec![])];
        let held = unheld(&["A"]);

        let conditions = survey(&tasks, &held);

        assert_eq!(
            conditions.get(&TaskId::from_string("A")),
            Some(&Condition::Failed)
        );
    }

    #[test]
    fn an_in_progress_task_no_one_holds_is_an_orphan() {
        let tasks = vec![task("A", TaskStatus::InProgress, vec![])];
        let held = unheld(&["A"]);

        let conditions = survey(&tasks, &held);

        assert_eq!(
            conditions.get(&TaskId::from_string("A")),
            Some(&Condition::Orphaned)
        );
    }

    #[test]
    fn an_in_progress_task_someone_holds_is_not_an_orphan() {
        // MONITOR's liveness windows decide whether held work is a problem;
        // REPLAN's survey only sees that it is not stopped.
        let tasks = vec![task("A", TaskStatus::InProgress, vec![])];
        let mut held = unheld(&["A"]);
        held.insert(TaskId::from_string("A"), true);

        let conditions = survey(&tasks, &held);

        assert!(conditions.is_empty());
    }

    #[test]
    fn a_task_whose_dependency_failed_has_a_broken_premise() {
        let tasks = vec![
            task("A", TaskStatus::Failed, vec![]),
            task("B", TaskStatus::Todo, vec![TaskId::from_string("A")]),
        ];
        let held = unheld(&["A", "B"]);

        let conditions = survey(&tasks, &held);

        // The failed task is reported as failed — its own verdict is the more
        // actionable one — and the dependent as premise-broken.
        assert_eq!(
            conditions.get(&TaskId::from_string("A")),
            Some(&Condition::Failed)
        );
        assert_eq!(
            conditions.get(&TaskId::from_string("B")),
            Some(&Condition::PremiseBroken {
                dependency: TaskId::from_string("A"),
                dependency_status: TaskStatus::Failed
            })
        );
    }

    #[test]
    fn a_task_whose_dependency_was_cancelled_has_a_broken_premise() {
        let tasks = vec![
            task("A", TaskStatus::Cancelled, vec![]),
            task("B", TaskStatus::Todo, vec![TaskId::from_string("A")]),
        ];
        let held = unheld(&["A", "B"]);

        let conditions = survey(&tasks, &held);

        assert_eq!(
            conditions.get(&TaskId::from_string("B")),
            Some(&Condition::PremiseBroken {
                dependency: TaskId::from_string("A"),
                dependency_status: TaskStatus::Cancelled
            })
        );
    }

    #[test]
    fn a_task_whose_dependency_is_done_has_no_broken_premise() {
        let tasks = vec![
            task("A", TaskStatus::Done, vec![]),
            task("B", TaskStatus::Todo, vec![TaskId::from_string("A")]),
        ];
        let held = unheld(&["A", "B"]);

        let conditions = survey(&tasks, &held);

        assert!(conditions.is_empty());
    }

    #[test]
    fn a_task_whose_dependency_is_still_in_flight_has_no_broken_premise() {
        // The premise may still hold; the dependency has not verdicted yet.
        let tasks = vec![
            task("A", TaskStatus::InProgress, vec![]),
            task("B", TaskStatus::Todo, vec![TaskId::from_string("A")]),
        ];
        let held = unheld(&["A", "B"]);

        let conditions = survey(&tasks, &held);

        // A is held by no one, so it *is* an orphan — but B's premise is intact.
        assert_eq!(
            conditions.get(&TaskId::from_string("A")),
            Some(&Condition::Orphaned)
        );
        assert!(!conditions.contains_key(&TaskId::from_string("B")));
    }

    #[test]
    fn a_premise_breaks_on_the_first_dead_end_dependency() {
        // The report names the dependency that broke it, so the caller knows
        // which edge to answer.
        let tasks = vec![
            task("A", TaskStatus::Done, vec![]),
            task("B", TaskStatus::Failed, vec![]),
            task(
                "C",
                TaskStatus::Todo,
                vec![TaskId::from_string("A"), TaskId::from_string("B")],
            ),
        ];
        let held = unheld(&["A", "B", "C"]);

        let conditions = survey(&tasks, &held);

        assert_eq!(
            conditions.get(&TaskId::from_string("C")),
            Some(&Condition::PremiseBroken {
                dependency: TaskId::from_string("B"),
                dependency_status: TaskStatus::Failed
            })
        );
    }

    #[test]
    fn a_round_that_surveys_nothing_reports_nothing() {
        let tasks = vec![task("A", TaskStatus::Done, vec![])];
        let held = unheld(&["A"]);

        let report = ReplanReport {
            surveyed: survey(&tasks, &held)
                .into_iter()
                .map(|(task, condition)| Surveyed {
                    task,
                    condition: Some(condition),
                    outcome: Outcome::NotDecided,
                })
                .collect(),
        };

        assert!(report.surveyed.is_empty());
        assert!(report.is_settled());
    }

    #[test]
    fn a_retry_is_allowed_for_failed_and_orphaned_work() {
        let failed = task("A", TaskStatus::Failed, vec![]);
        let orphaned = task("B", TaskStatus::InProgress, vec![]);

        let retry = |id: &str| TaskDecision {
            task_id: TaskId::from_string(id),
            remediation: Remediation::Retry,
            reason: None,
        };

        assert!(validate_decision(&failed, &Condition::Failed, &retry("A")).is_ok());
        assert!(validate_decision(&orphaned, &Condition::Orphaned, &retry("B")).is_ok());
    }

    #[test]
    fn a_retry_of_a_broken_premise_is_refused() {
        // The task would go back to `todo` and never be handed out, because
        // `is_ready_given` still requires the dead-end dependency to be `Done`.
        let task = task("B", TaskStatus::Todo, vec![TaskId::from_string("A")]);
        let condition = Condition::PremiseBroken {
            dependency: TaskId::from_string("A"),
            dependency_status: TaskStatus::Failed,
        };
        let retry = TaskDecision {
            task_id: TaskId::from_string("B"),
            remediation: Remediation::Retry,
            reason: None,
        };

        let err = validate_decision(&task, &condition, &retry).unwrap_err();
        assert_eq!(
            err,
            RemediationError::RetryOnBrokenPremise {
                dependency: TaskId::from_string("A")
            }
        );
    }

    #[test]
    fn a_rework_of_a_broken_premise_is_allowed() {
        // Reworking the task is how the caller answers a broken premise: amend
        // the contract so the task no longer depends on the dead end.
        let task = task("B", TaskStatus::Todo, vec![TaskId::from_string("A")]);
        let condition = Condition::PremiseBroken {
            dependency: TaskId::from_string("A"),
            dependency_status: TaskStatus::Failed,
        };
        let rework = TaskDecision {
            task_id: TaskId::from_string("B"),
            remediation: Remediation::Rework {
                objective: Some("do it without A".to_string()),
                expected_outputs: None,
            },
            reason: None,
        };

        assert!(validate_decision(&task, &condition, &rework).is_ok());
    }

    #[test]
    fn a_cancel_is_allowed_for_any_condition() {
        // Abandoning work is always an available answer, whatever is wrong with
        // it.
        let failed = task("A", TaskStatus::Failed, vec![]);
        let orphaned = task("B", TaskStatus::InProgress, vec![]);
        let broken = task("C", TaskStatus::Todo, vec![TaskId::from_string("A")]);
        let broken_condition = Condition::PremiseBroken {
            dependency: TaskId::from_string("A"),
            dependency_status: TaskStatus::Failed,
        };

        let cancel = |id: &str| TaskDecision {
            task_id: TaskId::from_string(id),
            remediation: Remediation::Cancel,
            reason: Some("not worth it".to_string()),
        };

        assert!(validate_decision(&failed, &Condition::Failed, &cancel("A")).is_ok());
        assert!(validate_decision(&orphaned, &Condition::Orphaned, &cancel("B")).is_ok());
        assert!(validate_decision(&broken, &broken_condition, &cancel("C")).is_ok());
    }

    #[test]
    fn a_rework_that_changes_nothing_is_refused() {
        // A rework with no amendment is a retry wearing a different name.
        let task = task("A", TaskStatus::Failed, vec![]);
        let noop = TaskDecision {
            task_id: TaskId::from_string("A"),
            remediation: Remediation::Rework {
                objective: None,
                expected_outputs: None,
            },
            reason: None,
        };

        let err = validate_decision(&task, &Condition::Failed, &noop).unwrap_err();
        assert_eq!(err, RemediationError::NoOpRework);
    }

    #[test]
    fn a_rework_that_clears_expected_outputs_is_a_real_change() {
        // `Some(vec![])` empties the outputs rather than leaving them, which is
        // a change the caller might mean.
        let task = task("A", TaskStatus::Failed, vec![]);
        let clear = TaskDecision {
            task_id: TaskId::from_string("A"),
            remediation: Remediation::Rework {
                objective: None,
                expected_outputs: Some(vec![]),
            },
            reason: None,
        };

        assert!(validate_decision(&task, &Condition::Failed, &clear).is_ok());
    }

    #[test]
    fn remediations_map_to_statuses() {
        assert_eq!(
            remediation_destination(&Remediation::Retry),
            TaskStatus::Todo
        );
        assert_eq!(
            remediation_destination(&Remediation::Rework {
                objective: None,
                expected_outputs: None
            }),
            TaskStatus::Todo
        );
        assert_eq!(
            remediation_destination(&Remediation::Cancel),
            TaskStatus::Cancelled
        );
    }

    #[test]
    fn failed_and_cancelled_are_dead_ends_and_done_is_not() {
        assert!(is_dead_end(TaskStatus::Failed));
        assert!(is_dead_end(TaskStatus::Cancelled));
        assert!(!is_dead_end(TaskStatus::Done));
        assert!(!is_dead_end(TaskStatus::InProgress));
    }

    #[test]
    fn an_empty_report_is_settled() {
        // Nothing is stopped, and nothing stopped is waiting on a decision.
        let report = ReplanReport::default();
        assert!(report.is_settled());
        assert_eq!(report.retried().count(), 0);
        assert_eq!(report.cancelled().count(), 0);
    }

    #[test]
    fn the_accessors_partition_a_mixed_round() {
        let report = ReplanReport {
            surveyed: vec![
                Surveyed {
                    task: TaskId::from_string("A"),
                    condition: Some(Condition::Failed),
                    outcome: Outcome::Applied {
                        remediation: Remediation::Retry,
                        status: TaskStatus::Todo,
                    },
                },
                Surveyed {
                    task: TaskId::from_string("B"),
                    condition: Some(Condition::Failed),
                    outcome: Outcome::Applied {
                        remediation: Remediation::Rework {
                            objective: Some("new".to_string()),
                            expected_outputs: None,
                        },
                        status: TaskStatus::Todo,
                    },
                },
                Surveyed {
                    task: TaskId::from_string("C"),
                    condition: Some(Condition::PremiseBroken {
                        dependency: TaskId::from_string("X"),
                        dependency_status: TaskStatus::Cancelled,
                    }),
                    outcome: Outcome::Applied {
                        remediation: Remediation::Cancel,
                        status: TaskStatus::Cancelled,
                    },
                },
                Surveyed {
                    task: TaskId::from_string("D"),
                    condition: Some(Condition::Orphaned),
                    outcome: Outcome::Refused {
                        error: RemediationError::RetryOnBrokenPremise {
                            dependency: TaskId::from_string("X"),
                        },
                    },
                },
                Surveyed {
                    task: TaskId::from_string("E"),
                    condition: Some(Condition::PremiseBroken {
                        dependency: TaskId::from_string("C"),
                        dependency_status: TaskStatus::Cancelled,
                    }),
                    outcome: Outcome::NotDecided,
                },
                Surveyed {
                    task: TaskId::from_string("F"),
                    condition: None,
                    outcome: Outcome::NothingNeeded,
                },
            ],
        };

        // Retried counts both remediations that put work back in flight.
        assert_eq!(
            report.retried().map(|id| id.as_str()).collect::<Vec<_>>(),
            vec!["A", "B"]
        );
        assert_eq!(
            report.cancelled().map(|id| id.as_str()).collect::<Vec<_>>(),
            vec!["C"]
        );
        assert_eq!(
            report
                .refused()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["D"]
        );
        assert_eq!(
            report
                .undecided()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["E"]
        );
        assert!(!report.is_settled());
    }

    #[test]
    fn a_settled_round_with_no_surveyed_tasks_is_settled() {
        // An empty report and a fully applied one both satisfy `is_settled`;
        // the difference is whether anything needed deciding.
        let report = ReplanReport {
            surveyed: vec![Surveyed {
                task: TaskId::from_string("A"),
                condition: Some(Condition::Failed),
                outcome: Outcome::Applied {
                    remediation: Remediation::Retry,
                    status: TaskStatus::Todo,
                },
            }],
        };
        assert!(report.is_settled());
        assert_eq!(report.undecided().count(), 0);
    }
}
