//! Plan continuity: two tasks in one project, both recoverable.
//!
//! # What is real
//!
//! Real SQLite, real git, a real spawned agent, and the real planning, assignment
//! and verification paths. The agent is a locally compiled stand-in that speaks
//! Claude Code's stdio protocol — a real program, but not Claude Code.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use director_domain::ids::{PlanId, ProjectId, TaskId};
use director_domain::store::{
    AgentRepository, AssignmentRepository, PlanRepository, TaskRepository,
};
use director_domain::task::TaskStatus;

#[allow(dead_code)]
#[path = "../src/config.rs"]
mod config;
#[allow(dead_code)]
#[path = "../src/lifecycle.rs"]
mod lifecycle;

use crate::config::{Command as CliCommand, RunArgs};
use crate::lifecycle::ResumeAction;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

fn fixture_binary() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("orqynn-plan-agent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let source = dir.join("agent.rs");
        let binary = dir.join(if cfg!(windows) { "agent.exe" } else { "agent" });
        std::fs::write(&source, FIXTURE_SOURCE).expect("fixture source");
        let status = Command::new("rustc")
            .arg("--edition")
            .arg("2021")
            .arg("-O")
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .status()
            .expect("rustc on PATH");
        assert!(status.success(), "the agent fixture must compile");
        binary
    })
    .clone()
}

const FIXTURE_SOURCE: &str = r#"
fn main() {
    use std::io::Read;
    let mut brief = String::new();
    let _ = std::io::stdin().read_to_string(&mut brief);
    println!("{{\"type\":\"result\",\"is_error\":false,\"result\":\"did the work\"}}");
}
"#;

fn driver_for(timeout: Duration) -> director_adapters::harness::ClaudeCodeDriver {
    director_adapters::harness::ClaudeCodeDriver::new(
        director_adapters::harness::ClaudeCodeConfig {
            binary: fixture_binary().to_string_lossy().into_owned(),
            default_timeout: timeout,
            ..director_adapters::harness::ClaudeCodeConfig::default()
        },
    )
}

fn git_repo(dir: &Path) {
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("git on PATH");
        assert!(status.success(), "git {args:?} must succeed");
    };
    run(&["init", "-q"]);
    run(&["config", "user.name", "Orqynn Tests"]);
    run(&["config", "user.email", "tests@orqyn.invalid"]);
    run(&["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("README.md"), "# fixture\n").expect("write");
    run(&["add", "README.md"]);
    run(&["commit", "-q", "-m", "initial"]);
}

/// A scratch project plus the plumbing to add tasks and resume them.
struct Project {
    _tmp: tempfile::TempDir,
    args: RunArgs,
}

impl Project {
    async fn new() -> Self {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");
        git_repo(&repo);

        let mut args = RunArgs::parse([
            "--repo",
            repo.to_str().expect("utf-8"),
            "--task",
            "placeholder",
            "--check",
            "git --no-such-flag",
        ])
        .expect("parses");
        args.agent_binary = fixture_binary().to_string_lossy().into_owned();
        args.resolve().expect("resolves");
        Project { _tmp: tmp, args }
    }

    /// Plan and drive one task through the real `run` lifecycle.
    async fn run_task(&self, task: &str) -> RunArgs {
        let mut args = self.args.clone();
        args.task = task.to_string();
        args.task_id = String::new();
        args.checks = vec!["git --no-such-flag".into()];
        args.resolve().expect("resolves");

        let runtime = lifecycle::open_runtime(&args).await.expect("runtime");
        let driver = driver_for(Duration::from_secs(30));
        lifecycle::run_lifecycle(
            &runtime,
            &director_adapters::InMemoryProvider::new(),
            &driver,
            &director_adapters::executor::LocalExecutor::default(),
            &args,
            chrono::Utc::now(),
        )
        .await
        .expect("the run");
        args
    }

    async fn resume_args(&self, task_id: &str, retry: bool) -> RunArgs {
        let mut args = self.args.clone();
        args.command = CliCommand::Resume;
        args.task_id = task_id.to_string();
        args.checks.clear();
        if retry {
            args.remediate = Some(config::Remediation::Retry);
        }
        args
    }

    async fn resume(
        &self,
        task_id: &str,
        retry: bool,
    ) -> Result<lifecycle::ResumeReport, lifecycle::RunError> {
        let args = self.resume_args(task_id, retry).await;
        let runtime = lifecycle::open_runtime(&args).await.expect("runtime");
        let driver = driver_for(Duration::from_secs(30));
        lifecycle::resume_lifecycle(
            &runtime,
            &director_adapters::InMemoryProvider::new(),
            &driver,
            &director_adapters::executor::LocalExecutor::default(),
            &args,
            chrono::Utc::now(),
        )
        .await
    }

    async fn active_plan(&self) -> Option<PlanId> {
        let runtime = lifecycle::open_runtime(&self.args).await.expect("runtime");
        runtime
            .store
            .plans()
            .active_plan_for_project(&lifecycle::project_id())
            .await
            .expect("plans")
            .map(|plan| plan.id)
    }

    async fn all_plans(&self) -> Vec<director_domain::plan::Plan> {
        let runtime = lifecycle::open_runtime(&self.args).await.expect("runtime");
        runtime
            .store
            .plans()
            .plans_for_project(&lifecycle::project_id())
            .await
            .expect("plans")
    }
}

// ---------------------------------------------------------------------------
// 1 & 2. Two tasks, one plan, both recoverable
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_task_joins_the_active_plan_instead_of_superseding_it() {
    let project = Project::new().await;
    project.run_task("the first task here").await;
    let first_plan = project.active_plan().await.expect("a plan exists");

    project.run_task("the second task here").await;
    let after = project.active_plan().await.expect("a plan is active");

    assert_eq!(
        first_plan, after,
        "adding a task must not replace the plan the project is executing against"
    );
    let plans = project.all_plans().await;
    assert_eq!(
        plans.len(),
        1,
        "no second plan was created: {:?}",
        plans.iter().map(|p| p.id.clone()).collect::<Vec<_>>()
    );
    assert_eq!(
        plans[0].task_ids.len(),
        2,
        "both tasks belong to the one active plan"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn running_a_second_task_does_not_make_the_first_unresumable() {
    let project = Project::new().await;
    let first = project.run_task("the first task here").await;
    let first_id = first.task_id().to_string();

    // Without the fix this task would no longer be named by the active plan, and
    // the retry would be refused before an agent could be launched.
    project.run_task("the second task here").await;

    let report = project.resume(&first_id, true).await.expect("a report");
    assert_eq!(
        report.action,
        ResumeAction::Redriven,
        "an older task must remain recoverable: {}",
        report.lines.join("\n")
    );
    assert!(report
        .lines
        .join("\n")
        .contains("is now waiting to be handed out"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_older_task_survives_a_reopen_and_keeps_its_plan_membership() {
    let project = Project::new().await;
    let first = project.run_task("the first task here").await;
    let first_id = TaskId::from_string(first.task_id());
    project.run_task("the second task here").await;

    // A completely fresh handle onto the same file.
    let reopened = director_store::Store::open(&project.args.database)
        .await
        .expect("the store reopens");
    let plan = reopened
        .plans()
        .active_plan_for_project(&ProjectId::from_string("PROJ-1"))
        .await
        .expect("plans")
        .expect("an active plan");
    assert!(
        plan.task_ids.contains(&first_id),
        "plan membership survived the reopen: {:?}",
        plan.task_ids
    );
    let task = reopened
        .tasks()
        .get_task(&first_id)
        .await
        .expect("the task reads back");
    assert_eq!(
        task.status,
        TaskStatus::Failed,
        "and the older task kept its recorded status"
    );
}

// ---------------------------------------------------------------------------
// 3. Refusals unchanged
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_active_assignment_still_prevents_duplicate_execution() {
    let project = Project::new().await;
    let task = project.run_task("the first task here").await;

    // Put the task back in flight with an active assignment, which is what an
    // interrupted run leaves behind.
    let runtime = lifecycle::open_runtime(&project.args)
        .await
        .expect("runtime");
    let mut stored = runtime
        .store
        .tasks()
        .get_task(&TaskId::from_string(task.task_id()))
        .await
        .expect("task");
    stored.status = TaskStatus::InProgress;
    runtime
        .store
        .tasks()
        .update_task(&stored)
        .await
        .expect("updated");
    let agent = runtime
        .store
        .agents()
        .get_agent(&director_domain::ids::AgentId::from_string("AGENT-1"))
        .await
        .expect("agent");
    runtime
        .store
        .assignments()
        .create_assignment(&director_domain::assignment::AgentAssignment {
            id: director_domain::ids::AssignmentId::from_string("ASG-LIVE-1"),
            task_id: stored.id.clone(),
            agent_id: agent.id.clone(),
            session_id: None,
            status: director_domain::assignment::AssignmentStatus::Active,
            assigned_at: chrono::Utc::now(),
            released_at: None,
            release_reason: None,
            note: None,
            state_version: 0,
        })
        .await
        .expect("assignment created");

    let report = project
        .resume(task.task_id(), true)
        .await
        .expect("a report");
    assert_eq!(report.action, ResumeAction::Refused);
    assert!(report.lines.join("\n").contains("active assignment"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocked_task_is_still_refused() {
    let project = Project::new().await;
    let task = project.run_task("the first task here").await;
    let runtime = lifecycle::open_runtime(&project.args)
        .await
        .expect("runtime");
    let mut stored = runtime
        .store
        .tasks()
        .get_task(&TaskId::from_string(task.task_id()))
        .await
        .expect("task");
    stored.status = TaskStatus::Blocked;
    runtime
        .store
        .tasks()
        .update_task(&stored)
        .await
        .expect("updated");

    let report = project
        .resume(task.task_id(), true)
        .await
        .expect("a report");
    assert_eq!(report.action, ResumeAction::Refused);
    assert!(report.lines.join("\n").contains("blocked"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_done_task_is_still_refused_after_the_plan_change() {
    let project = Project::new().await;
    let task = project.run_task("the first task here").await;
    // The task failed by construction; mark it done the way a passing verdict
    // would, and confirm the terminal refusal still holds.
    let runtime = lifecycle::open_runtime(&project.args)
        .await
        .expect("runtime");
    let mut stored = runtime
        .store
        .tasks()
        .get_task(&TaskId::from_string(task.task_id()))
        .await
        .expect("task");
    stored.status = TaskStatus::Done;
    runtime
        .store
        .tasks()
        .update_task(&stored)
        .await
        .expect("updated");
    project.run_task("the second task here").await;

    let report = project
        .resume(task.task_id(), true)
        .await
        .expect("a report");
    assert_eq!(report.action, ResumeAction::Refused);
    assert!(report.lines.join("\n").contains("already done"));
}

// ---------------------------------------------------------------------------
// 4. History, duplicates, and cross-project isolation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plans_for_the_project_remain_inspectable() {
    let project = Project::new().await;
    project.run_task("the first task here").await;
    project.run_task("the second task here").await;

    let plans = project.all_plans().await;
    assert_eq!(plans.len(), 1);
    let plan = &plans[0];
    assert_eq!(plan.status, director_domain::plan::PlanStatus::Active);
    assert_eq!(
        plan.task_ids.len(),
        2,
        "the plan is a complete record of the project's work"
    );
    assert!(
        plan.id.as_str().starts_with("PLAN-"),
        "{}",
        plan.id.as_str()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeating_a_run_creates_no_duplicate_task() {
    let project = Project::new().await;
    let task = project.run_task("the first task here").await;
    let before = project.all_plans().await[0].task_ids.len();

    // Asking about the same task again must not add it to the plan a second time.
    // Without an authorised remediation the resume refuses, and a refusal writes
    // nothing at all — which is the stronger guarantee.
    let outcome = project.resume(task.task_id(), false).await;
    if let Ok(report) = outcome {
        // Also fine: a refusal writes nothing, which is the stronger guarantee.
        assert_eq!(
            report.action,
            ResumeAction::Refused,
            "a repeat with no remediation must change nothing: {}",
            report.lines.join("\n")
        );
    }
    assert_eq!(
        project.all_plans().await[0].task_ids.len(),
        before,
        "a refused repeat left plan membership untouched"
    );
    assert_eq!(
        project.all_plans().await[0]
            .task_ids
            .iter()
            .filter(|id| id.as_str() == task.task_id())
            .count(),
        1,
        "and the task is named exactly once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_task_cannot_be_added_to_another_projects_plan() {
    let project = Project::new().await;
    project.run_task("the first task here").await;
    let runtime = lifecycle::open_runtime(&project.args)
        .await
        .expect("runtime");

    // A plan belonging to a project nobody observed is not a target for work.
    let foreign = director_domain::plan::Plan::draft(
        PlanId::from_string("PLAN-FOREIGN"),
        ProjectId::from_string("PROJ-SOMEWHERE-ELSE"),
        "someone else's work",
        "not this project",
    );
    let err = runtime
        .store
        .append_task_to_plan(
            &foreign.id,
            &director_domain::task::Task::for_project(
                ProjectId::from_string("PROJ-SOMEWHERE-ELSE"),
                TaskId::from_string("FOREIGN-TASK"),
                "foreign",
                "foreign work",
            ),
            "",
            chrono::Utc::now(),
        )
        .await;
    assert!(
        err.is_err(),
        "a plan for another project must not accept this project's work"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_output_distinguishes_a_new_plan_from_an_extended_one() {
    let project = Project::new().await;

    // First task: a plan is created, and the output says so.
    let mut first_args = project.args.clone();
    first_args.task = "the first task here".into();
    first_args.task_id = String::new();
    first_args.resolve().expect("resolves");
    let runtime = lifecycle::open_runtime(&first_args).await.expect("runtime");
    let first = lifecycle::run_lifecycle(
        &runtime,
        &director_adapters::InMemoryProvider::new(),
        &driver_for(Duration::from_secs(30)),
        &director_adapters::executor::LocalExecutor::default(),
        &first_args,
        chrono::Utc::now(),
    )
    .await
    .expect("the first run");
    let first_text = first.lines.join("\n");
    assert!(
        first_text.contains("created the project's first plan"),
        "the first task must say a plan was created: {first_text}"
    );
    assert!(
        !first_text.contains("no plan was superseded"),
        "and must not claim it extended anything: {first_text}"
    );

    // Second task: the report must name the existing plan, preserve its
    // originating objective, and show the accumulated membership.
    let mut second_args = project.args.clone();
    second_args.task = "the second task here".into();
    second_args.task_id = String::new();
    second_args.resolve().expect("resolves");
    let runtime = lifecycle::open_runtime(&second_args)
        .await
        .expect("runtime");
    let second = lifecycle::run_lifecycle(
        &runtime,
        &director_adapters::InMemoryProvider::new(),
        &driver_for(Duration::from_secs(30)),
        &director_adapters::executor::LocalExecutor::default(),
        &second_args,
        chrono::Utc::now(),
    )
    .await
    .expect("the second run");
    let second_text = second.lines.join("\n");
    assert!(
        second_text.contains("no plan was superseded"),
        "the second task must say it extended: {second_text}"
    );
    assert!(
        second_text.contains("still records its originating objective"),
        "and must distinguish origin from accumulated scope: {second_text}"
    );
    assert!(
        second_text.contains("plan member:"),
        "and must list what the plan now covers: {second_text}"
    );

    let plan = &project.all_plans().await[0];
    assert_eq!(plan.task_ids.len(), 2);
    assert_eq!(plan.objective, "the first task here");
    assert!(
        plan.rationale.contains("scope extended"),
        "the extended scope is recorded: {}",
        plan.rationale
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_runs_add_tasks_without_ever_rewriting_the_original_objective() {
    let project = Project::new().await;
    project.run_task("the first task here").await;
    let objective = project.all_plans().await[0].objective.clone();
    assert_eq!(objective, "the first task here");

    for (n, task) in ["the second task here", "the third task here"]
        .iter()
        .enumerate()
    {
        project.run_task(task).await;
        let plan = &project.all_plans().await[0];
        assert_eq!(
            plan.objective,
            objective,
            "run {} rewrote the originating objective",
            n + 2
        );
        assert_eq!(plan.task_ids.len(), n + 2, "one member per requested task");
        assert_eq!(
            plan.task_ids
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            plan.task_ids.len(),
            "no task is a member twice"
        );
    }

    // One plan, one note per extension, and the notes are in the record.
    let plan = &project.all_plans().await[0];
    assert_eq!(
        plan.rationale
            .lines()
            .filter(|l| l.contains("scope extended"))
            .count(),
        2
    );
    assert!(plan.rationale.starts_with("one task, requested explicitly"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_still_refuses_a_done_task_after_the_plan_grows() {
    // The Phase 8 guard, re-asserted in the Phase 9/10 shape: an older task in a
    // larger plan is still terminal once it is done, and is not reopened.
    let project = Project::new().await;
    let task = project.run_task("the first task here").await;
    let runtime = lifecycle::open_runtime(&project.args)
        .await
        .expect("runtime");
    let mut stored = runtime
        .store
        .tasks()
        .get_task(&TaskId::from_string(task.task_id()))
        .await
        .expect("task");
    stored.status = TaskStatus::Done;
    runtime
        .store
        .tasks()
        .update_task(&stored)
        .await
        .expect("updated");

    project.run_task("the second task here").await;
    project.run_task("the third task here").await;

    let report = project
        .resume(task.task_id(), true)
        .await
        .expect("a report");
    assert_eq!(report.action, ResumeAction::Refused);
    assert!(report.lines.join("\n").contains("already done"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_existing_invocation_still_works_unchanged() {
    // Backward compatibility: the documented `run` invocation, with no resume
    // involved, still plans, drives and verifies.
    let project = Project::new().await;
    let args = project.args.clone();
    let runtime = lifecycle::open_runtime(&args).await.expect("runtime");
    let report = lifecycle::run_lifecycle(
        &runtime,
        &director_adapters::InMemoryProvider::new(),
        &driver_for(Duration::from_secs(30)),
        &director_adapters::executor::LocalExecutor::default(),
        &args,
        chrono::Utc::now(),
    )
    .await
    .expect("the run");
    assert_eq!(report.exit_code, lifecycle::ExitCode::VerificationFailed);
    assert!(report.lines.join("\n").contains("planned 1 task(s)"));
}
