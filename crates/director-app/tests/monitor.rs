//! Integration tests for the MONITOR step against a real SQLite store.
//!
//! These are integration tests for the same reason OBSERVE's and ASSIGN's are:
//! the risk in this step is not that `monitor` calls the right methods, but
//! that the layers line up. A mock could confirm that `liveness` was consulted;
//! it cannot show that the heartbeat age the store computes and the windows the
//! domain derives agree, or that a lease expiry releases the tenure, closes the
//! session, and returns the task to `todo` together. So every test here works
//! against a real store on disk and asserts by *reloading* the records, because
//! the store is what the next loop tick reads.
//!
//! The clock is pinned to `now` in every test. Liveness is derived from
//! heartbeat age, and the difference between `Working`, `Stale`, and `Gone` is
//! thirty and sixty minutes — waiting for that would be an hour per test, so
//! the tests move the heartbeats instead of the clock.

use director_app::assign::{assign, ready_tasks, AssignRequest};
use director_app::monitor::{
    acknowledge, monitor_at, validate_acknowledgment, AcknowledgeRequest, InFlight,
};
use director_app::MonitorError;
use director_domain::agent::{Agent, AgentStatus, Harness};
use director_domain::assignment::{AssignmentStatus, ReleaseReason};
use director_domain::capability::Capability;
use director_domain::ids::{
    AgentId, AssignmentId, MachineId, PlanId, ProjectId, SessionId, TaskId,
};
use director_domain::plan::Plan;
use director_domain::project::Project;
use director_domain::session::{SessionEnd, SessionStatus};
use director_domain::task::{Task, TaskStatus};
use director_domain::{
    AgentRepository, AssignmentRepository, ProjectRepository, SessionRepository, TaskRepository,
};
use director_store::Store;

/// A store, a project with an active plan, and a registered agent — everything
/// MONITOR needs to have something in flight.
struct Fixture {
    store: Store,
    _dir: tempfile::TempDir,
    project: ProjectId,
    agent: AgentId,
    now: chrono::DateTime<chrono::Utc>,
}

impl Fixture {
    /// A store with one project, one active plan holding `tasks`, and one
    /// available agent.
    async fn new(tasks: &[&str]) -> Self {
        let dir = tempfile::TempDir::new().expect("a temp dir for the store");
        let store = Store::open(dir.path().join("orqyn.db"))
            .await
            .expect("store opens");

        let project = ProjectId::from_string("PROJ-1");
        store
            .projects()
            .create_project(&Project::new(project.clone(), project.as_str(), "/nowhere"))
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
            .map(|id| {
                let mut task =
                    Task::for_project(project.clone(), TaskId::from_string(*id), *id, "do it");
                task.status = TaskStatus::Todo;
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

        Fixture {
            store,
            _dir: dir,
            project,
            agent,
            now: chrono::Utc::now(),
        }
    }

    /// Hand a task to the fixture's agent, the way ASSIGN does.
    async fn assign(&self, task: &str) {
        assign(
            &self.store,
            AssignRequest {
                project_id: self.project.clone(),
                task_id: TaskId::from_string(task),
                agent_id: self.agent.clone(),
                assignment_id: AssignmentId::from_string("ASG-1"),
            },
        )
        .await
        .expect("assigned");
    }

    /// Move the agent's last heartbeat `minutes` into the past, the way a quiet
    /// agent's registry entry looks to a later round.
    async fn quiet_for(&self, minutes: i64) {
        let mut agent = self
            .store
            .agents()
            .get_agent(&self.agent)
            .await
            .expect("agent");
        agent.last_seen = self.now - chrono::Duration::minutes(minutes);
        self.store
            .agents()
            .update_agent(&agent)
            .await
            .expect("agent updated");
    }

    async fn reload_task(&self, id: &str) -> Task {
        self.store
            .tasks()
            .get_task(&TaskId::from_string(id))
            .await
            .expect("task exists")
    }

    async fn reload_agent(&self) -> Agent {
        self.store
            .agents()
            .get_agent(&self.agent)
            .await
            .expect("agent exists")
    }
}

#[tokio::test]
async fn a_fresh_heartbeat_leaves_the_lease_standing() {
    let fixture = Fixture::new(&["A"]).await;
    fixture.assign("A").await;

    let report = monitor_at(&fixture.store, &fixture.project, fixture.now)
        .await
        .expect("the round");

    // The agent just took the task, so the round is quiet: it surveyed the work
    // and had nothing to do about it.
    assert!(report.is_quiet());
    assert_eq!(report.in_flight.len(), 1, "one task is in flight");
    match &report.in_flight[0] {
        InFlight::Live {
            task,
            agent,
            liveness,
        } => {
            assert_eq!(task.as_str(), "A");
            assert_eq!(agent, &fixture.agent);
            assert_eq!(*liveness, director_domain::agent::Liveness::Working);
        }
        other => panic!("expected a live lease, got {other:?}"),
    }

    // And nothing was written: the task is still being worked and the tenure
    // still holds it.
    assert_eq!(
        fixture.reload_task("A").await.status,
        TaskStatus::InProgress
    );
    assert_eq!(
        fixture
            .store
            .assignments()
            .active_assignment_for_task(&TaskId::from_string("A"))
            .await
            .expect("the query")
            .expect("there is one")
            .status,
        AssignmentStatus::Active
    );
}

#[tokio::test]
async fn a_stale_heartbeat_is_reported_but_the_lease_stands() {
    let fixture = Fixture::new(&["A"]).await;
    fixture.assign("A").await;
    // Quiet for 45 minutes: past the fresh window, not yet past the stale one.
    fixture.quiet_for(45).await;

    let report = monitor_at(&fixture.store, &fixture.project, fixture.now)
        .await
        .expect("the round");

    // `Stale` is a report, not an intervention: a quiet agent may be thinking,
    // and ending its lease early is work that has to restart.
    assert!(report.is_quiet());
    assert!(matches!(
        report.in_flight[0],
        InFlight::Live {
            liveness: director_domain::agent::Liveness::Stale,
            ..
        }
    ));
    assert_eq!(
        fixture.reload_task("A").await.status,
        TaskStatus::InProgress
    );
    assert_eq!(
        fixture.reload_agent().await.status,
        AgentStatus::Available,
        "a stale heartbeat does not change the agent's own status"
    );
}

#[tokio::test]
async fn an_agent_past_the_stale_window_loses_the_lease() {
    let fixture = Fixture::new(&["A"]).await;
    fixture.assign("A").await;
    // Quiet for 90 minutes: past the stale window, so the lease is over.
    fixture.quiet_for(90).await;

    let report = monitor_at(&fixture.store, &fixture.project, fixture.now)
        .await
        .expect("the round");

    let expired = report.expired();
    assert_eq!(expired.len(), 1, "one lease was reclaimed");
    assert_eq!(expired[0].task.id.as_str(), "A");

    // The task is handable again.
    assert_eq!(fixture.reload_task("A").await.status, TaskStatus::Todo);

    // The tenure is retained — not deleted — and its reason says the lease ran
    // out rather than that someone chose to reassign.
    let tenure = fixture
        .store
        .assignments()
        .assignment_history(&TaskId::from_string("A"))
        .await
        .expect("history");
    assert_eq!(tenure.len(), 1, "the tenure is retained");
    assert_eq!(tenure[0].status, AssignmentStatus::Released);
    assert_eq!(tenure[0].release_reason, Some(ReleaseReason::LeaseExpired));

    // The agent holds nothing and is recorded as gone, so the loop will not
    // hand the work straight back to it.
    let agent = fixture.reload_agent().await;
    assert_eq!(agent.current_task, None);
    assert_eq!(agent.status, AgentStatus::Disconnected);

    // And no assignment is in force.
    assert!(fixture
        .store
        .assignments()
        .active_assignment_for_task(&TaskId::from_string("A"))
        .await
        .expect("the query")
        .is_none());
}

#[tokio::test]
async fn an_expired_lease_is_handable_again() {
    // The point of reclaiming a lease: the next ASSIGN round can hand the work
    // out again, to an agent that is still there.
    let fixture = Fixture::new(&["A"]).await;
    fixture.assign("A").await;
    fixture.quiet_for(90).await;

    monitor_at(&fixture.store, &fixture.project, fixture.now)
        .await
        .expect("the round");

    let ready = ready_tasks(&fixture.store, &fixture.project)
        .await
        .expect("ready tasks");
    let ids: Vec<_> = ready.iter().map(|task| task.id.as_str()).collect();
    assert_eq!(ids, vec!["A"], "the reclaimed task is handable again");
}

#[tokio::test]
async fn a_second_round_after_an_expiry_has_nothing_to_reclaim() {
    // Idempotency: an expired lease moved the task back to `todo`, so the next
    // round does not even survey it.
    let fixture = Fixture::new(&["A"]).await;
    fixture.assign("A").await;
    fixture.quiet_for(90).await;

    let first = monitor_at(&fixture.store, &fixture.project, fixture.now)
        .await
        .expect("the first round");
    assert_eq!(first.expired().len(), 1);

    let second = monitor_at(&fixture.store, &fixture.project, fixture.now)
        .await
        .expect("the second round");
    assert!(second.in_flight.is_empty(), "nothing is in flight any more");
    assert!(second.is_quiet());
}

#[tokio::test]
async fn an_in_progress_task_with_no_tenant_is_reported_orphaned() {
    // The state a stranded handoff would leave behind: a task reading
    // `in_progress` with no assignment behind it. MONITOR reports it and writes
    // nothing — deciding what to do with tenantless work is REPLAN's job.
    let fixture = Fixture::new(&["A"]).await;
    let mut task = fixture.reload_task("A").await;
    task.status = TaskStatus::InProgress;
    fixture
        .store
        .tasks()
        .update_task(&task)
        .await
        .expect("task moved to in progress");

    let report = monitor_at(&fixture.store, &fixture.project, fixture.now)
        .await
        .expect("the round");

    assert!(!report.is_quiet());
    assert_eq!(
        report.orphaned(),
        vec![&TaskId::from_string("A")],
        "the task with no tenant is reported"
    );
    assert_eq!(
        fixture.reload_task("A").await.status,
        TaskStatus::InProgress,
        "the round did not touch it"
    );
}

#[tokio::test]
async fn a_round_surveys_only_the_work_in_flight() {
    let fixture = Fixture::new(&["A", "B"]).await;
    fixture.assign("A").await;
    // B stays `todo`: nothing is holding it, so it is not MONITOR's business.
    fixture.quiet_for(90).await;

    let report = monitor_at(&fixture.store, &fixture.project, fixture.now)
        .await
        .expect("the round");

    assert_eq!(report.in_flight.len(), 1, "only the in-flight task");
    assert_eq!(report.expired().len(), 1);
    assert_eq!(fixture.reload_task("B").await.status, TaskStatus::Todo);
}

#[tokio::test]
async fn acknowledgment_records_the_session_and_marks_the_agent_busy() {
    let fixture = Fixture::new(&["A"]).await;
    fixture.assign("A").await;

    let acknowledged = acknowledge(
        &fixture.store,
        AcknowledgeRequest {
            assignment_id: AssignmentId::from_string("ASG-1"),
            session_id: SessionId::from_string("SESS-1"),
        },
    )
    .await
    .expect("acknowledged");

    // The tenure now has the invocation doing the work behind it, and the
    // session agrees with the records it was derived from.
    assert_eq!(
        acknowledged.assignment.session_id,
        Some(SessionId::from_string("SESS-1"))
    );
    assert_eq!(acknowledged.session.task_id, Some(TaskId::from_string("A")));
    assert_eq!(acknowledged.session.agent_id, fixture.agent);

    // Both reload — the store is what the next round reads.
    assert_eq!(
        fixture
            .store
            .assignments()
            .get_assignment(&AssignmentId::from_string("ASG-1"))
            .await
            .expect("assignment")
            .session_id,
        Some(SessionId::from_string("SESS-1"))
    );
    assert_eq!(
        fixture
            .store
            .sessions()
            .get_session(&SessionId::from_string("SESS-1"))
            .await
            .expect("session")
            .status,
        SessionStatus::Active
    );
    assert_eq!(
        fixture.reload_agent().await.status,
        AgentStatus::Busy,
        "an agent that started its work is working"
    );
}

#[tokio::test]
async fn an_acknowledged_session_is_closed_as_vanished_when_the_lease_runs_out() {
    // The two halves of MONITOR compose: a session the agent reported, closed by
    // a lease the agent let run out.
    let fixture = Fixture::new(&["A"]).await;
    fixture.assign("A").await;
    acknowledge(
        &fixture.store,
        AcknowledgeRequest {
            assignment_id: AssignmentId::from_string("ASG-1"),
            session_id: SessionId::from_string("SESS-1"),
        },
    )
    .await
    .expect("acknowledged");
    fixture.quiet_for(90).await;

    let report = monitor_at(&fixture.store, &fixture.project, fixture.now)
        .await
        .expect("the round");

    let expired = report.expired();
    assert_eq!(expired.len(), 1);
    let session = expired[0]
        .session
        .as_ref()
        .expect("the lease had a session");
    assert_eq!(session.end, Some(SessionEnd::Vanished));
    assert!(session.ended_uncleanly());
}

#[tokio::test]
async fn acknowledging_an_unknown_assignment_is_refused() {
    let fixture = Fixture::new(&["A"]).await;
    fixture.assign("A").await;

    let err = acknowledge(
        &fixture.store,
        AcknowledgeRequest {
            assignment_id: AssignmentId::from_string("ASG-nope"),
            session_id: SessionId::from_string("SESS-1"),
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, MonitorError::UnknownAssignment(ref id)
            if *id == AssignmentId::from_string("ASG-nope")),
        "got {err:?}"
    );

    // Nothing was recorded.
    assert!(
        fixture
            .store
            .sessions()
            .get_session(&SessionId::from_string("SESS-1"))
            .await
            .is_err(),
        "no session was written"
    );
}

#[tokio::test]
async fn acknowledging_a_tenure_twice_is_refused() {
    let fixture = Fixture::new(&["A"]).await;
    fixture.assign("A").await;
    acknowledge(
        &fixture.store,
        AcknowledgeRequest {
            assignment_id: AssignmentId::from_string("ASG-1"),
            session_id: SessionId::from_string("SESS-1"),
        },
    )
    .await
    .expect("first acknowledgment");

    let err = acknowledge(
        &fixture.store,
        AcknowledgeRequest {
            assignment_id: AssignmentId::from_string("ASG-1"),
            session_id: SessionId::from_string("SESS-2"),
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, MonitorError::AlreadyAcknowledged { ref assignment, ref session }
            if *assignment == AssignmentId::from_string("ASG-1")
            && *session == SessionId::from_string("SESS-1")),
        "got {err:?}"
    );

    // The second session was never stored; the first is the evidence trail.
    assert!(
        fixture
            .store
            .sessions()
            .get_session(&SessionId::from_string("SESS-2"))
            .await
            .is_err(),
        "no second session was written"
    );
}

#[tokio::test]
async fn acknowledging_a_tenure_the_lease_expired_is_refused() {
    // A tenure that ran out cannot gain evidence of work it never did, so the
    // refusal names the assignment rather than the session.
    let fixture = Fixture::new(&["A"]).await;
    fixture.assign("A").await;
    fixture.quiet_for(90).await;
    monitor_at(&fixture.store, &fixture.project, fixture.now)
        .await
        .expect("the lease expired");

    let err = acknowledge(
        &fixture.store,
        AcknowledgeRequest {
            assignment_id: AssignmentId::from_string("ASG-1"),
            session_id: SessionId::from_string("SESS-1"),
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, MonitorError::AssignmentNotActive(ref id)
            if *id == AssignmentId::from_string("ASG-1")),
        "got {err:?}"
    );

    assert!(
        fixture
            .store
            .sessions()
            .get_session(&SessionId::from_string("SESS-1"))
            .await
            .is_err(),
        "no session was written against the ended tenure"
    );
}

#[test]
fn the_acknowledgment_rules_are_the_pure_half_of_the_step() {
    // The rules a store cannot be built to check, checked without one.
    let mut assignment = director_domain::assignment::AgentAssignment::propose(
        AssignmentId::from_string("ASG-1"),
        TaskId::from_string("A"),
        AgentId::from_string("AGENT-1"),
    );
    assert!(
        validate_acknowledgment(&assignment).is_err(),
        "proposed, not active"
    );
    assignment.activate(SessionId::from_string("SESS-1"));
    assert!(
        validate_acknowledgment(&assignment).is_err(),
        "already has a session"
    );
}
