//! Live integration against a real handoff-mcp server.
//!
//! These are `#[ignore]` by default: they spawn the server binary and touch
//! the filesystem, so they cannot run in CI where the binary may not exist.
//! Run them explicitly:
//!
//! ```sh
//! HANDOFF_MCP_BINARY=/path/to/handoff-mcp(.exe) \
//!   cargo test -p director-adapters --test handoff_live -- --ignored --nocapture
//! ```
//!
//! ## Why these exist
//!
//! The unit tests in `handoff/adapter.rs` drive a fake that mirrors what the
//! substrate *should* do. Only a live run can catch the real failure mode
//! these guard against: the wire mirror in `handoff/wire.rs` drifting from the
//! server's actual JSON, so a reply parses to nothing or to the wrong thing.
//! Every shape these tests parse was verified against the live server once;
//! a change upstream that breaks them is a real regression, not a flake.

use std::path::PathBuf;

use director_adapters::HandoffAdapter;
use director_domain::agent::{Agent, Harness};
use director_domain::ids::{AgentId, MachineId, SessionId, TaskId};
use director_domain::providers::{AgentProvider, SessionProvider, TaskProvider};
use director_domain::session::{AgentSession, SessionEnd, SessionStatus};
use director_domain::task::{Task, TaskStatus};

/// Where the built server lives on this machine, overridable for other setups.
fn binary() -> String {
    std::env::var("HANDOFF_MCP_BINARY")
        .unwrap_or_else(|_| "C:/Users/ASUS/.handoff-target/release/handoff-mcp.exe".to_string())
}

/// A fresh, throwaway project directory per test run, so no test sees another's
/// state. Not under the repo: the substrate writes a `.handoff/` directory, and
/// a sync engine taking locks there would make these flaky.
fn scratch_dir(name: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("director-live-{}-{}", name, std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

async fn connect(name: &str, agent: &str) -> HandoffAdapter {
    let dir = scratch_dir(name);
    std::fs::remove_dir_all(dir.join(".handoff")).ok();
    let adapter = HandoffAdapter::connect(
        &binary(),
        AgentId::from_string(agent.to_string()),
        dir,
        name,
    )
    .await
    .expect("connect to the live server");
    // Register so the identity exists for the agent assertions.
    adapter
        .register_agent(Agent::register(
            adapter.agent_identity(),
            agent,
            Harness::Other("director-live".into()),
            MachineId::from_string("MACH-live".to_string()),
            vec![],
        ))
        .await
        .expect("register");
    adapter
}

fn task(id: &str) -> Task {
    Task::new(
        TaskId::from_string(id.to_string()),
        format!("Live {}", id),
        format!("Objective for {}", id),
    )
}

#[tokio::test]
#[ignore]
async fn a_task_round_trips_through_the_live_server() {
    let adapter = connect("round-trip", "AGENT-live-1").await;

    let created = adapter
        .create_task(task("LIVE-1"))
        .await
        .expect("create over the wire");
    assert_eq!(created.id, TaskId::from_string("LIVE-1".to_string()));
    assert_eq!(created.status, TaskStatus::Todo);
    assert_eq!(created.objective, "Objective for LIVE-1");

    let back = adapter
        .get_task(&TaskId::from_string("LIVE-1".to_string()))
        .await
        .expect("get")
        .expect("the task exists");
    assert_eq!(back.id, created.id);
    assert_eq!(back.title, "Live LIVE-1");

    let listed: Vec<String> = adapter
        .list_tasks()
        .await
        .expect("list")
        .into_iter()
        .map(|t| t.id.to_string())
        .collect();
    assert!(listed.contains(&"LIVE-1".to_string()));
}

#[tokio::test]
#[ignore]
async fn an_agent_done_does_not_complete_but_a_director_done_does() {
    // The acceptance criterion, live: an agent reporting done must not
    // complete the task, while Director's own completion must.
    let adapter = connect("done-rule", "AGENT-live-2").await;
    let id = TaskId::from_string("LIVE-2".to_string());

    adapter.create_task(task("LIVE-2")).await.expect("create");

    // The agent's self-report: done written straight through the substrate,
    // bypassing Director's own write path the way an agent would.
    adapter
        .raw_call(
            "handoff_update_task",
            serde_json::json!({ "task": { "id": "LIVE-2", "status": "done" } }),
        )
        .await
        .expect("the agent reports done");

    let reported = adapter.get_task(&id).await.expect("get").expect("exists");
    assert_eq!(
        reported.status,
        TaskStatus::VerificationPending,
        "an agent's done is not Director's Done"
    );

    // Director's own completion, through the provider trait.
    adapter
        .set_task_status(&id, TaskStatus::Done)
        .await
        .expect("director completes");
    let completed = adapter.get_task(&id).await.expect("get").expect("exists");
    assert_eq!(completed.status, TaskStatus::Done);
}

#[tokio::test]
#[ignore]
async fn a_task_with_criteria_can_be_completed_live() {
    // The substrate rejects a done transition with an unchecked criterion, so
    // this is also a live check that the adapter's write repair works.
    let adapter = connect("criteria", "AGENT-live-3").await;
    let id = TaskId::from_string("LIVE-3".to_string());

    let mut task = task("LIVE-3");
    task.expected_outputs
        .push(director_domain::task::ExpectedOutput {
            criterion: "the live server accepts this".into(),
            check: None,
        });
    adapter.create_task(task).await.expect("create");

    adapter
        .set_task_status(&id, TaskStatus::Done)
        .await
        .expect("the completion is not rejected");
    assert_eq!(
        adapter
            .get_task(&id)
            .await
            .expect("get")
            .expect("exists")
            .status,
        TaskStatus::Done
    );
}

#[tokio::test]
#[ignore]
async fn the_live_agent_and_session_round_trip() {
    let adapter = connect("agents-sessions", "AGENT-live-4").await;

    let agents = adapter.list_agents().await.expect("list agents");
    assert!(agents.iter().any(|a| a.id == adapter.agent_identity()));

    let started = adapter
        .start_session(AgentSession::start(
            SessionId::from_string("director-issued".to_string()),
            adapter.agent_identity(),
            MachineId::from_string("MACH-live".to_string()),
            None,
        ))
        .await
        .expect("start session");
    assert_eq!(started.status, SessionStatus::Active);

    adapter
        .close_session(&started.id, SessionEnd::Clean)
        .await
        .expect("close");
    assert_eq!(
        adapter
            .get_session(&started.id)
            .await
            .expect("get")
            .expect("exists")
            .status,
        SessionStatus::Closed
    );
}

#[tokio::test]
#[ignore]
async fn an_unsupported_agent_operation_fails_loudly() {
    let adapter = connect("unsupported", "AGENT-live-5").await;
    let other = AgentId::from_string("AGENT-someone-else".to_string());
    let error = adapter
        .register_agent(Agent::register(
            other,
            "someone else",
            Harness::Other("test".into()),
            MachineId::from_string("MACH-x".to_string()),
            vec![],
        ))
        .await
        .expect_err("registering another identity must fail");
    assert!(format!("{error}").contains("speaks as"));
}
