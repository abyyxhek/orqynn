//! Multi-agent coordination audit probes, application layer (Phase 42).
//!
//! The companion of `director-store/tests/multi_agent_audit.rs`. That file
//! probes whether the *store* holds the coordination invariants; this one
//! probes whether the application layer's promises about the same operations
//! survive concurrency.
//!
//! The specific claim under test is the one `assign.rs` makes about a sitting
//! tenant: `assign` refuses rather than displacing the holding agent silently.
//! The refusal is an optimistic pre-check — read the active assignment, refuse
//! if there is one — and the write that follows is the store's unconditional
//! handoff. Two callers that both read before either writes both pass the
//! check, and the later one displaces the earlier one's assignment instead of
//! being refused.
//!
//! Untracked audit evidence, expected to stay green.

use std::sync::{Arc, Barrier};

use director_app::assign::{assign_at, AssignRequest};
use director_app::plan::{PlanRequest, TaskSpec};
use director_domain::agent::{Agent, AgentStatus, Harness};
use director_domain::capability::Capability;
use director_domain::ids::{AgentId, AssignmentId, MachineId, PlanId, ProjectId, TaskId};
use director_domain::project::Project;
use director_domain::task::TaskStatus;
use director_domain::{AgentRepository, AssignmentRepository, ProjectRepository, TaskRepository};
use director_store::Store;

/// A pinned instant, so the audit's history is the history the test wrote.
fn pinned() -> chrono::DateTime<chrono::Utc> {
    chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 11, 3, 9, 0, 0).unwrap()
}

/// Drive one future on the calling thread, the way the store's concurrency
/// tests do.
fn run<T, F>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime")
        .block_on(future)
}

/// One project, one planned scoped task, and two registered agents — the state
/// the concurrent-assignment claim needs.
async fn fixture() -> (Store, Store, TaskId, AgentId, AgentId) {
    let dir = tempfile::TempDir::new().expect("a temp dir for the audit store");
    let path = dir.path().join("orqyn.db");
    std::mem::forget(dir);
    let store = Store::open(&path).await.expect("store opens");
    let other = Store::open(&path)
        .await
        .expect("a second store on the same file");

    let project = ProjectId::from_string("PROJ-1");
    store
        .projects()
        .create_project(&Project::new(project.clone(), "proj", "/nowhere"))
        .await
        .expect("project created");

    let task = TaskId::from_string("AUTH-1");
    director_app::plan::plan(
        &store,
        PlanRequest {
            project_id: project.clone(),
            plan_id: PlanId::from_string("PLAN-1"),
            objective: "the audit's task".into(),
            rationale: "the app-layer probe needs an active plan".into(),
            tasks: vec![TaskSpec {
                id: task.clone(),
                title: "AUTH-1".into(),
                objective: "the audit's task".into(),
                depends_on: vec![],
                expected_outputs: vec![],
                priority: None,
                complexity: director_domain::task::Complexity::Unknown,
                scope_paths: vec!["src/module-a/".into()],
            }],
            authorized_by: AgentId::from_string("AGENT-planner"),
        },
    )
    .await
    .expect("plan succeeds");

    async fn register(store: &Store, name: &str) -> AgentId {
        let id = AgentId::from_string(name);
        let mut agent = Agent::register(
            id.clone(),
            name,
            Harness::ClaudeCode,
            MachineId::from_string("MACH-a"),
            vec![Capability::Coding],
        );
        agent.status = AgentStatus::Available;
        store
            .agents()
            .register_agent(&agent)
            .await
            .expect("agent registered");
        id
    }
    let agent_a = register(&store, "AGENT-a").await;
    let agent_b = register(&store, "AGENT-b").await;

    (store, other, task, agent_a, agent_b)
}

/// Two concurrent `assign` calls for the *same* task must not both silently
/// succeed: the module documents a refusal when another agent already holds
/// the task, so one of the two callers should see [`AssignError::TaskAlreadyAssigned`].
///
/// What actually happens: both callers pass the optimistic pre-check, and the
/// store's handoff displaces the first assignment. The unambiguity invariant
/// (one active assignment per task) holds — that is the partial unique index —
/// but the *refusal* the application layer documents does not survive
/// concurrency. This test records the behavior as it stands.
#[test]
fn two_concurrent_assigns_of_one_task_both_report_success() {
    let (store, other, task, agent_a, agent_b) = run(async { fixture().await });

    let request = |agent: AgentId, id: &str| AssignRequest {
        project_id: ProjectId::from_string("PROJ-1"),
        task_id: task.clone(),
        agent_id: agent,
        assignment_id: AssignmentId::from_string(id),
    };
    let request_a = request(agent_a.clone(), "ASG-A");
    let request_b = request(agent_b.clone(), "ASG-B");

    let store_for_b = other.clone();
    let store_for_a = store.clone();
    let gate = Arc::new(Barrier::new(2));
    let gate_b = gate.clone();
    let a = std::thread::spawn(move || {
        run(async move {
            gate.wait();
            assign_at(&store_for_a, request_a, pinned()).await
        })
    });
    let b = std::thread::spawn(move || {
        run(async move {
            gate_b.wait();
            assign_at(&store_for_b, request_b, pinned()).await
        })
    });
    let a_outcome = a.join().expect("A finishes");
    let b_outcome = b.join().expect("B finishes");

    // What the audit records: both succeed. If this ever changes to one
    // `TaskAlreadyAssigned` refusal, the check-then-act window is closed and
    // this test should be updated to assert it.
    let a_assigned = a_outcome.is_ok();
    let b_assigned = b_outcome.is_ok();
    assert!(
        a_assigned && b_assigned,
        "the optimistic pre-check races the handoff: expected both assignments to report success, \
         got A={a_assigned:?} ({a_outcome:?}), B={b_assigned:?} ({b_outcome:?})"
    );

    // The invariant that *does* hold: one active assignment, and the task is
    // in flight exactly once.
    let active = run(async {
        store
            .assignments()
            .assignment_history(&task)
            .await
            .expect("history reads")
            .into_iter()
            .filter(|a| a.status == director_domain::assignment::AssignmentStatus::Active)
            .collect::<Vec<_>>()
    });
    assert_eq!(active.len(), 1, "the unambiguity rule holds even here");
    let task_row = run(async { store.tasks().get_task(&task).await.expect("reads") });
    assert_eq!(task_row.status, TaskStatus::InProgress);
    // A displaced B or B displaced A: whichever it is, the other tenure survives
    // in the history as `released` with the reassignment recorded.
    let history = run(async {
        store
            .assignments()
            .assignment_history(&task)
            .await
            .expect("tenures read")
    });
    assert_eq!(
        history.len(),
        2,
        "both tenures are recorded; neither is lost"
    );
    let released = history
        .iter()
        .filter(|a| a.status == director_domain::assignment::AssignmentStatus::Released)
        .count();
    assert_eq!(released, 1, "the loser's tenure is retained, not deleted");
}
