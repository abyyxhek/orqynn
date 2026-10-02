//! Integration tests for the REPLAN step against a real SQLite store.
//!
//! REPLAN answers for work the loop's own verdicts stopped, so these tests
//! bring tasks to the states it reads the way the running loop produces them —
//! ASSIGN hands a task out and MONITOR's lease expiry strands one as an
//! orphan, a plan arrives with a dependency edge so a cancellation can break a
//! premise — rather than a status being poked by hand. The states REPLAN reads
//! are the states those steps produce, and a hand-built `failed` row does not
//! prove the round fits the loop.
//!
//! Every assertion reloads the record from the store, because the point of a
//! replan round is what the *next* loop tick sees.

use director_app::assign::{assign, AssignRequest};
use director_app::monitor::monitor_at;
use director_app::replan::{
    replan, Condition, Outcome, Remediation, RemediationError, ReplanReport, ReplanRequest,
    Surveyed, TaskDecision,
};
use director_domain::agent::{Agent, Harness};
use director_domain::assignment::AssignmentStatus;
use director_domain::capability::Capability;
use director_domain::decision::Decision;
use director_domain::ids::{
    AgentId, AssignmentId, DecisionId, MachineId, PlanId, ProjectId, TaskId,
};
use director_domain::plan::Plan;
use director_domain::project::Project;
use director_domain::task::{ExpectedOutput, Task, TaskStatus};
use director_domain::{
    AgentRepository, AssignmentRepository, DecisionRepository, ProjectRepository, TaskRepository,
};
use director_store::Store;

/// One task the caller named, and what to do with it.
///
/// The module's own [`TaskDecision`] is the public name; this is the ergonomic
/// way to write one in a test.
fn decide(task: &str, remediation: Remediation) -> TaskDecision {
    TaskDecision {
        task_id: TaskId::from_string(task),
        remediation,
        reason: Some("test reason".to_string()),
    }
}

/// A store, a project with an active plan holding the given tasks, and one
/// available agent — everything every step of the loop needs, set up the way
/// the steps before REPLAN leave it.
struct Fixture {
    store: Store,
    project: ProjectId,
    agent: AgentId,
}

/// One task's shape for the fixture's plan: an id, a dependency list, and the
/// expected outputs VERIFY will judge it by.
type TaskSpec = (String, Vec<TaskId>, Vec<ExpectedOutput>);

impl Fixture {
    /// A store with one project, one active plan holding the given tasks, and
    /// one available agent.
    async fn new(tasks: &[TaskSpec]) -> Self {
        let dir = tempfile::TempDir::new().expect("a temp dir for the store");
        let store = Store::open(dir.path().join("orqyn.db"))
            .await
            .expect("store opens");

        let project = ProjectId::from_string("PROJ-1");
        store
            .projects()
            .create_project(&Project::new(
                project.clone(),
                project.as_str(),
                dir.path().to_str().unwrap_or("/nowhere"),
            ))
            .await
            .expect("project created");

        let plan_id = PlanId::from_string("PLAN-1");
        let mut plan = Plan::draft(
            plan_id,
            project.clone(),
            "add authentication",
            "test rationale",
        );
        let built: Vec<Task> = tasks
            .iter()
            .map(|(id, deps, outputs)| {
                let mut task =
                    Task::for_project(project.clone(), TaskId::from_string(id), id, "do it");
                task.status = TaskStatus::Todo;
                task.dependencies = deps.clone();
                task.expected_outputs = outputs.clone();
                task
            })
            .collect();
        for task in &built {
            plan.add_task(task.id.clone());
        }
        store
            .create_active_plan(
                &plan,
                &built,
                &AgentId::from_string("AGENT-planner"),
                chrono::Utc::now(),
            )
            .await
            .expect("plan created and activated");

        let agent = AgentId::from_string("AGENT-1");
        store
            .agents()
            .register_agent(&Agent::register(
                agent.clone(),
                "claude-a",
                Harness::ClaudeCode,
                MachineId::from_string("MACH-a"),
                vec![Capability::Coding],
            ))
            .await
            .expect("agent registered");

        // The temp dir is dropped here, which would delete the database; the
        // store holds its own copy of the path and the pool keeps the file
        // open, so this is the fixture's intentional lifetime.
        drop(dir);

        Fixture {
            store,
            project,
            agent,
        }
    }

    /// Hand a task to the fixture's agent, the way ASSIGN does.
    async fn assign(&self, task: &str, assignment: &str) {
        assign(
            &self.store,
            AssignRequest {
                project_id: self.project.clone(),
                task_id: TaskId::from_string(task),
                agent_id: self.agent.clone(),
                assignment_id: AssignmentId::from_string(assignment),
            },
        )
        .await
        .expect("assigned");
    }

    /// Strand a task as an orphan: hand it out, then let MONITOR expire the
    /// lease. The task goes back to `todo` — which is *not* an orphan, so this
    /// is the fixture for the case where MONITOR's expiry already handled it.
    async fn expire(&self, task: &str, assignment: &str) {
        self.assign(task, assignment).await;
        // The agent never heartbeats, so its liveness is gone at any positive
        // age; a `now` after the assignment is enough.
        monitor_at(
            &self.store,
            &self.project,
            chrono::Utc::now() + chrono::Duration::hours(3),
        )
        .await
        .expect("monitor round");
    }

    async fn reload_task(&self, id: &str) -> Task {
        self.store
            .tasks()
            .get_task(&TaskId::from_string(id))
            .await
            .expect("task exists")
    }

    /// Run a round against this fixture's project.
    async fn replan(&self, decisions: Vec<TaskDecision>) -> ReplanReport {
        replan(
            &self.store,
            ReplanRequest {
                project_id: self.project.clone(),
                decisions,
                decided_by: Some(self.agent.clone()),
            },
        )
        .await
        .expect("the round")
    }

    /// The conditions the round surveyed, as a map from task id, for tests that
    /// want to assert on what the round found rather than what it did.
    fn conditions(report: &ReplanReport) -> Vec<(String, Option<Condition>)> {
        report
            .surveyed
            .iter()
            .map(|entry| (entry.task.as_str().to_string(), entry.condition.clone()))
            .collect()
    }
}

/// The fixture's task spec for one task with no dependencies and no checks.
fn plain(task: &str) -> TaskSpec {
    (task.to_string(), vec![], vec![])
}

#[tokio::test]
async fn a_failed_task_retried_is_handable_again() {
    // The loop's basic recovery: VERIFY failed the work, the caller decides it
    // is still worth doing, and the task is back in the pool ASSIGN reads.
    let fixture = Fixture::new(&[plain("A")]).await;
    let mut task = fixture.reload_task("A").await;
    task.status = TaskStatus::Failed;
    fixture
        .store
        .tasks()
        .update_task(&task)
        .await
        .expect("task failed");

    let report = fixture.replan(vec![decide("A", Remediation::Retry)]).await;

    assert!(report.is_settled());
    assert_eq!(
        report.retried().map(|id| id.as_str()).collect::<Vec<_>>(),
        vec!["A"]
    );
    assert_eq!(fixture.reload_task("A").await.status, TaskStatus::Todo);
}

#[tokio::test]
async fn a_retried_task_is_not_resurveyed_next_round() {
    // Idempotency: the retried task is `todo`, which is none of the three
    // conditions, so the next round does not even survey it.
    let fixture = Fixture::new(&[plain("A")]).await;
    let mut task = fixture.reload_task("A").await;
    task.status = TaskStatus::Failed;
    fixture
        .store
        .tasks()
        .update_task(&task)
        .await
        .expect("task failed");

    fixture.replan(vec![decide("A", Remediation::Retry)]).await;
    let second = fixture.replan(vec![]).await;

    assert!(second.surveyed.is_empty(), "the retried task is done with");
}

#[tokio::test]
async fn a_failed_task_reworked_gets_the_amended_contract() {
    // Rework is not retry: the caller fixes the criterion, and the task carries
    // the new contract to the next agent that works it.
    let fixture = Fixture::new(&[plain("A")]).await;
    let mut task = fixture.reload_task("A").await;
    task.status = TaskStatus::Failed;
    fixture
        .store
        .tasks()
        .update_task(&task)
        .await
        .expect("task failed");

    let report = fixture
        .replan(vec![decide(
            "A",
            Remediation::Rework {
                objective: Some("login with OAuth".to_string()),
                expected_outputs: Some(vec![ExpectedOutput {
                    criterion: "OAuth flow completes".to_string(),
                    check: Some("git --version".to_string()),
                }]),
            },
        )])
        .await;

    assert!(report.is_settled());
    let reloaded = fixture.reload_task("A").await;
    assert_eq!(reloaded.status, TaskStatus::Todo);
    assert_eq!(reloaded.objective, "login with OAuth");
    assert_eq!(reloaded.expected_outputs.len(), 1);
    assert_eq!(
        reloaded.expected_outputs[0].criterion,
        "OAuth flow completes"
    );
}

#[tokio::test]
async fn a_cancelled_task_is_terminal_with_the_reason_recorded() {
    // The task is `cancelled`, and the reason outlives it as a decision — so
    // "why did we stop pursuing this" stays answerable.
    let fixture = Fixture::new(&[plain("A")]).await;
    let mut task = fixture.reload_task("A").await;
    task.status = TaskStatus::Failed;
    fixture
        .store
        .tasks()
        .update_task(&task)
        .await
        .expect("task failed");

    let report = fixture.replan(vec![decide("A", Remediation::Cancel)]).await;

    assert!(report.is_settled());
    assert_eq!(
        report.cancelled().map(|id| id.as_str()).collect::<Vec<_>>(),
        vec!["A"]
    );
    assert_eq!(fixture.reload_task("A").await.status, TaskStatus::Cancelled);

    let decisions = fixture
        .store
        .decisions()
        .decisions_for_task(&TaskId::from_string("A"))
        .await
        .expect("decisions");
    assert_eq!(decisions.len(), 1, "one decision per cancellation");
    assert!(decisions[0].title.contains("A"));
    assert_eq!(decisions[0].rationale, "test reason");
    assert_eq!(decisions[0].made_by, Some(fixture.agent.clone()));
}

#[tokio::test]
async fn the_verdict_lands_in_the_history_as_a_transition() {
    // A remediation is a status move, so it lands in the append-only history:
    // the trail says the task went to `todo` from `failed`.
    let fixture = Fixture::new(&[plain("A")]).await;
    let mut task = fixture.reload_task("A").await;
    task.status = TaskStatus::Failed;
    fixture
        .store
        .tasks()
        .update_task(&task)
        .await
        .expect("task failed");

    fixture.replan(vec![decide("A", Remediation::Retry)]).await;

    let history = fixture
        .store
        .tasks()
        .task_history(&TaskId::from_string("A"))
        .await
        .expect("history");
    let last = history.last().expect("there is a transition");
    assert_eq!(last.from, TaskStatus::Failed);
    assert_eq!(last.to, TaskStatus::Todo);
}

#[tokio::test]
async fn an_orphaned_task_the_loop_produced_is_surveyed() {
    // A task stranded with no tenant is the state MONITOR reports and leaves
    // standing. Here it is produced by the loop's own steps rather than by
    // writing a status: the assignment is released directly, leaving the task
    // `in_progress` with no one holding it.
    let fixture = Fixture::new(&[plain("A")]).await;
    fixture.assign("A", "ASG-1").await;
    let assignment = fixture
        .store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string("A"))
        .await
        .expect("the query")
        .expect("someone holds it");
    fixture
        .store
        .assignments()
        .release_assignment(
            &assignment.id,
            director_domain::assignment::ReleaseReason::Reassigned,
            assignment.state_version,
        )
        .await
        .expect("released");

    let report = fixture.replan(vec![]).await;

    assert_eq!(
        Fixture::conditions(&report),
        vec![("A".to_string(), Some(Condition::Orphaned))]
    );
    assert!(!report.is_settled(), "the orphan needs an answer");

    // And answering it puts the task back in flight.
    let answered = fixture.replan(vec![decide("A", Remediation::Retry)]).await;
    assert!(answered.is_settled());
    assert_eq!(fixture.reload_task("A").await.status, TaskStatus::Todo);
}

#[tokio::test]
async fn a_task_held_by_an_agent_is_not_surveyed() {
    // In-flight work with a live tenant is MONITOR's concern, not REPLAN's; the
    // survey finds nothing to decide.
    let fixture = Fixture::new(&[plain("A")]).await;
    fixture.assign("A", "ASG-1").await;

    let report = fixture.replan(vec![]).await;

    assert!(report.surveyed.is_empty());
    assert_eq!(
        fixture.reload_task("A").await.status,
        TaskStatus::InProgress
    );
}

#[tokio::test]
async fn an_expired_lease_is_already_back_to_todo_and_not_surveyed() {
    // MONITOR's expiry puts the task back to `todo` itself, which is none of
    // the three conditions — so REPLAN sees nothing to do. The two steps do not
    // overlap, and this is the proof.
    let fixture = Fixture::new(&[plain("A")]).await;
    fixture.expire("A", "ASG-1").await;

    let report = fixture.replan(vec![]).await;

    assert!(report.surveyed.is_empty());
    assert_eq!(fixture.reload_task("A").await.status, TaskStatus::Todo);
    // The tenure is retained, as every expiry retains it.
    let tenure = fixture
        .store
        .assignments()
        .assignment_history(&TaskId::from_string("A"))
        .await
        .expect("history");
    assert_eq!(tenure.len(), 1);
    assert_eq!(tenure[0].status, AssignmentStatus::Released);
}

#[tokio::test]
async fn cancelling_a_task_strands_its_dependents_in_the_report() {
    // The cascade: cancelling A breaks the premise of B, which depends on it.
    // B is reported, and written about not at all.
    let fixture = Fixture::new(&[
        plain("A"),
        ("B".to_string(), vec![TaskId::from_string("A")], vec![]),
    ])
    .await;
    let mut task = fixture.reload_task("A").await;
    task.status = TaskStatus::Failed;
    fixture
        .store
        .tasks()
        .update_task(&task)
        .await
        .expect("task failed");

    let report = fixture.replan(vec![decide("A", Remediation::Cancel)]).await;

    // A is cancelled; B is stranded and undecided.
    assert_eq!(
        report.cancelled().map(|id| id.as_str()).collect::<Vec<_>>(),
        vec!["A"]
    );
    let undecided: Vec<_> = report
        .undecided()
        .map(|(id, condition)| (id.as_str().to_string(), condition.clone()))
        .collect();
    assert_eq!(
        undecided,
        vec![(
            "B".to_string(),
            Condition::PremiseBroken {
                dependency: TaskId::from_string("A"),
                dependency_status: TaskStatus::Cancelled
            }
        )],
        "the round reports the dependent it stranded"
    );
    assert!(!report.is_settled(), "B still needs an answer");

    // And the round wrote nothing about B.
    assert_eq!(fixture.reload_task("B").await.status, TaskStatus::Todo);
}

#[tokio::test]
async fn cancelling_a_task_that_was_not_yet_a_dead_end_strands_a_new_dependent() {
    // The case the post-apply re-survey exists for. A is *orphaned*, not failed
    // — not a dead end — so B, depending on it, is healthy when the round
    // starts. Cancelling A is what breaks B's premise. The pre-apply survey
    // alone would never see B, because at that point nothing was wrong with it.
    let fixture = Fixture::new(&[
        plain("A"),
        ("B".to_string(), vec![TaskId::from_string("A")], vec![]),
    ])
    .await;
    // Strand A as an orphan: handed out, then released out from under it.
    fixture.assign("A", "ASG-1").await;
    let assignment = fixture
        .store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string("A"))
        .await
        .expect("the query")
        .expect("someone holds it");
    fixture
        .store
        .assignments()
        .release_assignment(
            &assignment.id,
            director_domain::assignment::ReleaseReason::Reassigned,
            assignment.state_version,
        )
        .await
        .expect("released");
    // B is healthy: its only dependency is orphaned, which is not a dead end.
    // A is surveyed as an orphan, but B must not appear anywhere in the report.
    let before = fixture.replan(vec![]).await;
    assert!(
        before
            .surveyed
            .iter()
            .all(|entry| entry.task != TaskId::from_string("B")),
        "B's premise is intact while its dependency is merely orphaned"
    );

    let report = fixture.replan(vec![decide("A", Remediation::Cancel)]).await;

    let undecided: Vec<_> = report
        .undecided()
        .map(|(id, condition)| (id.as_str().to_string(), condition.clone()))
        .collect();
    assert_eq!(
        undecided,
        vec![(
            "B".to_string(),
            Condition::PremiseBroken {
                dependency: TaskId::from_string("A"),
                dependency_status: TaskStatus::Cancelled
            }
        )],
        "the round reports the dependent *its own* cancellation stranded"
    );
    assert_eq!(fixture.reload_task("A").await.status, TaskStatus::Cancelled);
    assert_eq!(fixture.reload_task("B").await.status, TaskStatus::Todo);
}

#[tokio::test]
async fn a_stranded_dependent_is_not_handed_out_by_the_scheduler() {
    // The report-only cascade is safe because `ready_in_plan` will not hand out
    // a task whose dependency is not `Done`. This is the property that makes
    // reporting enough.
    let fixture = Fixture::new(&[
        plain("A"),
        ("B".to_string(), vec![TaskId::from_string("A")], vec![]),
    ])
    .await;
    let mut task = fixture.reload_task("A").await;
    task.status = TaskStatus::Failed;
    fixture
        .store
        .tasks()
        .update_task(&task)
        .await
        .expect("task failed");

    fixture.replan(vec![decide("A", Remediation::Cancel)]).await;

    let ready = director_app::assign::ready_tasks(&fixture.store, &fixture.project)
        .await
        .expect("the readiness read");
    assert!(
        ready.iter().all(|task| task.id != TaskId::from_string("B")),
        "a task with a cancelled dependency is never handed out"
    );
}

#[tokio::test]
async fn a_task_the_round_itself_strands_after_the_caller_named_it_is_reopened() {
    // The case the post-apply re-survey's outcome flip exists for. The caller
    // names B — healthy, because its dependency A is only *orphaned*, not a dead
    // end — and asks for a retry, which the round reports as `NothingNeeded`:
    // nothing was wrong with B, so nothing was applied. The same round cancels
    // A, and that cancellation is what breaks B's premise.
    //
    // Leaving B as `NothingNeeded` would be a self-contradicting report: an entry
    // carrying a real condition while `is_settled` called the round settled and
    // `undecided` skipped it. B has to be reopened as `NotDecided`, because the
    // answer the caller gave is no longer an answer to the question B is asking
    // now.
    let fixture = Fixture::new(&[
        plain("A"),
        ("B".to_string(), vec![TaskId::from_string("A")], vec![]),
    ])
    .await;
    // Strand A as an orphan: handed out, then released out from under it. An
    // orphan is not a dead end, so B's premise is intact at survey time.
    fixture.assign("A", "ASG-1").await;
    let assignment = fixture
        .store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string("A"))
        .await
        .expect("the query")
        .expect("someone holds it");
    fixture
        .store
        .assignments()
        .release_assignment(
            &assignment.id,
            director_domain::assignment::ReleaseReason::Reassigned,
            assignment.state_version,
        )
        .await
        .expect("released");

    let report = fixture
        .replan(vec![
            decide("A", Remediation::Cancel),
            decide("B", Remediation::Retry),
        ])
        .await;

    // A is cancelled; B is stranded by the round's own write, and reopened.
    assert_eq!(
        report.cancelled().map(|id| id.as_str()).collect::<Vec<_>>(),
        vec!["A"]
    );
    let undecided: Vec<_> = report
        .undecided()
        .map(|(id, condition)| (id.as_str().to_string(), condition.clone()))
        .collect();
    assert_eq!(
        undecided,
        vec![(
            "B".to_string(),
            Condition::PremiseBroken {
                dependency: TaskId::from_string("A"),
                dependency_status: TaskStatus::Cancelled
            }
        )],
        "the caller's healthy decision is reopened, not buried"
    );
    assert!(
        !report.is_settled(),
        "a task the round stranded is not settled just because the caller named it"
    );
    // And the round wrote nothing about B — its retry was never applied.
    assert_eq!(fixture.reload_task("B").await.status, TaskStatus::Todo);
}

#[tokio::test]
async fn a_retry_of_a_broken_premise_is_refused_and_writes_nothing() {
    // The round rejects the retry, and because one decision was refused it
    // applies none of them — including the one that would have been fine.
    let fixture = Fixture::new(&[
        plain("A"),
        ("B".to_string(), vec![TaskId::from_string("A")], vec![]),
    ])
    .await;
    let mut a = fixture.reload_task("A").await;
    a.status = TaskStatus::Failed;
    fixture
        .store
        .tasks()
        .update_task(&a)
        .await
        .expect("A failed");

    let report = fixture
        .replan(vec![
            decide("A", Remediation::Retry),
            decide("B", Remediation::Retry),
        ])
        .await;

    let refused: Vec<_> = report
        .refused()
        .map(|(id, error)| (id.as_str().to_string(), error.clone()))
        .collect();
    assert_eq!(
        refused,
        vec![(
            "B".to_string(),
            RemediationError::RetryOnBrokenPremise {
                dependency: TaskId::from_string("A")
            }
        )]
    );

    // All-or-nothing: A's retry was valid, but the round refused B, so neither
    // landed.
    assert_eq!(fixture.reload_task("A").await.status, TaskStatus::Failed);
    assert_eq!(fixture.reload_task("B").await.status, TaskStatus::Todo);
}

#[tokio::test]
async fn a_rework_answers_a_broken_premise() {
    // The caller amends the contract, and the task goes back to `todo`. Its
    // dependency is still a dead end, so the scheduler still will not hand it
    // out — but the round's job is to record the caller's answer, not to
    // second-guess it.
    let fixture = Fixture::new(&[
        plain("A"),
        ("B".to_string(), vec![TaskId::from_string("A")], vec![]),
    ])
    .await;
    let mut a = fixture.reload_task("A").await;
    a.status = TaskStatus::Failed;
    fixture
        .store
        .tasks()
        .update_task(&a)
        .await
        .expect("A failed");

    let report = fixture
        .replan(vec![decide(
            "B",
            Remediation::Rework {
                objective: Some("do it without A".to_string()),
                expected_outputs: None,
            },
        )])
        .await;

    // A is failed and undecided; B is reworked.
    assert_eq!(
        report.retried().map(|id| id.as_str()).collect::<Vec<_>>(),
        vec!["B"]
    );
    let reloaded = fixture.reload_task("B").await;
    assert_eq!(reloaded.status, TaskStatus::Todo);
    assert_eq!(reloaded.objective, "do it without A");
    // A is untouched, still waiting on the caller.
    assert_eq!(fixture.reload_task("A").await.status, TaskStatus::Failed);
}

#[tokio::test]
async fn a_decision_for_a_healthy_task_is_reported_not_applied() {
    // The caller decided a task nothing is wrong with. The round says so rather
    // than vanishing the decision, and writes nothing.
    let fixture = Fixture::new(&[plain("A")]).await;

    let report = fixture.replan(vec![decide("A", Remediation::Retry)]).await;

    assert_eq!(
        report.surveyed,
        vec![Surveyed {
            task: TaskId::from_string("A"),
            condition: None,
            outcome: Outcome::NothingNeeded,
        }]
    );
    assert!(report.is_settled(), "nothing is waiting on anything");
    assert_eq!(fixture.reload_task("A").await.status, TaskStatus::Todo);
}

#[tokio::test]
async fn a_decision_for_an_unknown_task_is_refused() {
    // A typo in a task id is an error, not a silent skip — otherwise the caller
    // would believe a decision landed that never did.
    let fixture = Fixture::new(&[plain("A")]).await;
    let mut task = fixture.reload_task("A").await;
    task.status = TaskStatus::Failed;
    fixture
        .store
        .tasks()
        .update_task(&task)
        .await
        .expect("task failed");

    let report = fixture
        .replan(vec![
            decide("A", Remediation::Retry),
            decide("NOPE", Remediation::Cancel),
        ])
        .await;

    assert_eq!(
        report
            .refused()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        vec!["NOPE"]
    );
    // All-or-nothing again: A's valid retry did not land either.
    assert_eq!(fixture.reload_task("A").await.status, TaskStatus::Failed);
}

#[tokio::test]
async fn a_task_decided_twice_is_refused() {
    // Two decisions for one task contradict each other; the round refuses
    // rather than picking one.
    let fixture = Fixture::new(&[plain("A")]).await;
    let mut task = fixture.reload_task("A").await;
    task.status = TaskStatus::Failed;
    fixture
        .store
        .tasks()
        .update_task(&task)
        .await
        .expect("task failed");

    let report = fixture
        .replan(vec![
            decide("A", Remediation::Retry),
            decide("A", Remediation::Cancel),
        ])
        .await;

    let refused: Vec<_> = report
        .refused()
        .map(|(id, error)| (id.as_str().to_string(), error.clone()))
        .collect();
    assert_eq!(
        refused,
        vec![("A".to_string(), RemediationError::DuplicateDecision)]
    );
    assert_eq!(fixture.reload_task("A").await.status, TaskStatus::Failed);
}

#[tokio::test]
async fn a_task_the_survey_found_that_the_caller_did_not_decide_is_left_alone() {
    // A partial round is legal: the caller answers what it has an answer for,
    // and the rest is reported and untouched.
    let fixture = Fixture::new(&[plain("A"), plain("B")]).await;
    let mut a = fixture.reload_task("A").await;
    a.status = TaskStatus::Failed;
    fixture.store.tasks().update_task(&a).await.unwrap();
    let mut b = fixture.reload_task("B").await;
    b.status = TaskStatus::Failed;
    fixture.store.tasks().update_task(&b).await.unwrap();

    let report = fixture.replan(vec![decide("A", Remediation::Retry)]).await;

    assert_eq!(fixture.reload_task("A").await.status, TaskStatus::Todo);
    assert_eq!(fixture.reload_task("B").await.status, TaskStatus::Failed);
    assert!(!report.is_settled(), "B is still waiting on a decision");
}

#[tokio::test]
async fn a_round_with_nothing_stopped_is_empty_and_settled() {
    // Nothing failed, nothing is orphaned, no premise is broken: the round has
    // nothing to report and nothing to do, which is the healthy-tick state.
    let fixture = Fixture::new(&[plain("A"), plain("B")]).await;

    let report = fixture.replan(vec![]).await;

    assert!(report.surveyed.is_empty());
    assert!(report.is_settled());
}

#[tokio::test]
async fn a_cancellation_whose_decision_cannot_be_written_leaves_the_task_standing() {
    // BUG-1 regression. The task's cancellation and the decision that records
    // why are one store transaction, so a round that cannot record the reason
    // cannot cancel the task either. Before the fix, the task write committed
    // and the decision write failed, leaving a `cancelled` task with no record
    // of why anyone abandoned it — a cancellation the store cannot explain.
    let fixture = Fixture::new(&[plain("A")]).await;
    let mut task = fixture.reload_task("A").await;
    task.status = TaskStatus::Failed;
    fixture
        .store
        .tasks()
        .update_task(&task)
        .await
        .expect("task failed");

    // Occupy the decision id the round will use. The task update would succeed
    // on its own; this is the write that fails, and it is the one the atomicity
    // guarantee has to protect.
    fixture
        .store
        .decisions()
        .create_decision(&Decision::new(
            DecisionId::from_string("DEC-CANCEL-A"),
            "an earlier decision",
            "already holds this id",
        ))
        .await
        .expect("the id is taken");

    let round = replan(
        &fixture.store,
        ReplanRequest {
            project_id: fixture.project.clone(),
            decisions: vec![decide("A", Remediation::Cancel)],
            decided_by: Some(fixture.agent.clone()),
        },
    )
    .await;

    assert!(round.is_err(), "the round fails rather than half-applying");
    // The task is exactly where the round found it: not cancelled.
    assert_eq!(
        fixture.reload_task("A").await.status,
        TaskStatus::Failed,
        "the task was not cancelled without its reason"
    );
    // And nothing was partially persisted: the round's decision did not land.
    let decisions = fixture
        .store
        .decisions()
        .decisions_for_task(&TaskId::from_string("A"))
        .await
        .expect("decisions");
    assert!(
        decisions.is_empty(),
        "the cancellation decision did not partially land"
    );
}

#[tokio::test]
async fn a_task_whose_premise_the_round_repaired_is_settled_not_undecided() {
    // BUG-2 regression. A is the dead end that breaks B's premise, and the
    // caller retries A in the same round. A goes back to `todo` — no longer a
    // dead end — so the post-apply re-survey finds no condition on B at all.
    //
    // B was reported as `NotDecided` while its premise was broken, and leaving
    // it that way makes the report contradict itself: `undecided` would skip it
    // because there is no condition to answer, while `is_settled` refused to
    // call the round settled over a task still marked undecided. A task the
    // round repaired has to go back to needing nothing.
    let fixture = Fixture::new(&[
        plain("A"),
        ("B".to_string(), vec![TaskId::from_string("A")], vec![]),
    ])
    .await;
    let mut a = fixture.reload_task("A").await;
    a.status = TaskStatus::Failed;
    fixture
        .store
        .tasks()
        .update_task(&a)
        .await
        .expect("A failed");

    let report = fixture.replan(vec![decide("A", Remediation::Retry)]).await;

    let b = report
        .surveyed
        .iter()
        .find(|entry| entry.task == TaskId::from_string("B"))
        .expect("B was surveyed while its premise was broken");

    assert_eq!(
        b.condition, None,
        "A is todo again, so B's premise is intact"
    );
    assert!(
        report.undecided().next().is_none(),
        "B has nothing left undecided about it"
    );
    assert!(report.refused().next().is_none());
    assert!(
        report.is_settled(),
        "a round that answered every condition it found is settled"
    );
}
