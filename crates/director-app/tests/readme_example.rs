// AUTO-GENERATED from README.md's Code Example section by
// tools/sync_readme_example.py. If this fails to compile or run, the
// README's example has drifted from the real API.

use director_adapters::git::GitService;
use director_adapters::{InMemoryProvider, LocalExecutor};
use director_app::observe::observe;
use director_app::plan::{plan, PlanRequest, TaskSpec};
use director_app::recovery::{RecoveryBuilder, RecoveryOutcome};
use director_app::schedule::schedule;
use director_app::verify::verify;
use director_domain::agent::{Agent, Harness};
use director_domain::capability::Capability;
use director_domain::ids::{
    AgentId, IdGenerator, MachineId, PlanId, ProjectId, RepositoryId, TaskId,
};
use director_domain::project::Project;
use director_domain::{AgentRepository, ProjectRepository};
use director_store::Store;
use tempfile::tempdir;

async fn readme_example_main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    std::fs::create_dir_all(dir.path().join("repo"))?;

    // 0. Orqyn observes a git working tree, so seed one for the demo.
    let repo_dir = dir.path().join("repo");
    for args in [
        &["init", "-q"][..],
        &["config", "user.name", "orqyn"][..],
        &["config", "user.email", "orqyn@demo"][..],
    ] {
        std::process::Command::new("git")
            .current_dir(&repo_dir)
            .args(args)
            .status()?;
    }
    // A repo with no commits has a HEAD pointing at nothing; give OBSERVE a
    // baseline commit to walk.
    std::fs::write(repo_dir.join("README.md"), "# orqyn demo\n")?;
    for args in [
        &["add", "README.md"][..],
        &["commit", "-q", "-m", "baseline"][..],
    ] {
        std::process::Command::new("git")
            .current_dir(&repo_dir)
            .args(args)
            .status()?;
    }

    // 1. Open the SQLite store and the read-only git lens over the working tree.
    let store = Store::open(dir.path().join("orqyn.db")).await?;
    let git = GitService::new(dir.path().join("gitstate"))?;

    let project = ProjectId::from_string("PROJ-1");
    let repository = RepositoryId::from_string("REPO-1");
    let local_path = repo_dir.to_string_lossy().replace('\\', "/");

    store
        .projects()
        .create_project(&Project::new(project.clone(), "orqyn-demo", &local_path))
        .await?;

    // 2. OBSERVE takes the baseline. The caller passes the id minter, so a
    //    restarted process keeps advancing ids instead of colliding on rows
    //    it already wrote.
    observe(
        &store,
        &git,
        &mut IdGenerator::new(),
        &InMemoryProvider::new(),
        &project,
        &repository,
        &local_path,
    )
    .await?;

    // 3. PLAN: two tasks in a DAG — the tests depend on the schema. The caller
    //    names the plan id and every task id; execution order is computed from
    //    the dependency graph, not assumed from the order of this list.
    let planned = plan(
        &store,
        PlanRequest {
            project_id: project.clone(),
            plan_id: PlanId::from_string("PLAN-1"),
            objective: "Ship user authentication".into(),
            rationale: "Schema before tests — the verifier needs both to run.".into(),
            tasks: vec![
                TaskSpec::new(
                    TaskId::from_string("MODEL"),
                    "Define auth schemas",
                    "POST /login returns 200 with a session cookie for a valid user",
                ),
                TaskSpec::new(
                    TaskId::from_string("TESTS"),
                    "Write auth unit tests",
                    "`cargo test --test auth` exits 0",
                )
                .depends_on(TaskId::from_string("MODEL"))
                .expects("login + token-verification tests pass"),
            ],
            authorized_by: AgentId::from_string("AGENT-1"),
        },
    )
    .await?;
    println!(
        "Plan {} active with {} tasks",
        planned.plan.id,
        planned.tasks.len()
    );

    // 4. Register the agent that will take the work, then SCHEDULE: the
    //    matchmaker pairs ready tasks with available agents and reports any
    //    ready work that no available agent can do.
    store
        .agents()
        .register_agent(&Agent::register(
            AgentId::from_string("AGENT-1"),
            "claude-code",
            Harness::ClaudeCode,
            MachineId::from_string("M-1"),
            vec![Capability::Coding],
        ))
        .await?;

    let scheduled = schedule(&store, &mut IdGenerator::new(), &project).await?;
    println!(
        "{} assignment(s) made, {} task(s) unassigned",
        scheduled.assigned.len(),
        scheduled.unassigned.len(),
    );

    // 5. VERIFY: Orqyn judges the work itself. Nothing an agent reports can
    //    reach `done` — a report of completion moves a task to
    //    `verification_pending`, and this step is the only thing that moves it
    //    further, by executing the probes and reading the exit codes.
    let report = verify(&store, &LocalExecutor::default(), &project, dir.path()).await?;
    println!("{} task(s) judged this round", report.judged.len());

    // 6. When an agent vanishes mid-task, brief its replacement by asking one
    //    question. `RecoveryBuilder` assembles the continuation package — task
    //    contract, checkpoint lineage, prior verdicts — and is the same code
    //    SCHEDULE uses when it hands recovered work to a new agent.
    let outcome = RecoveryBuilder::new(&store, TaskId::from_string("MODEL"))
        .from_stored_state()
        .build()
        .await?;

    match outcome {
        RecoveryOutcome::Built(recovery) => {
            println!("next action for a replacement: {}", recovery.next_action());
        }
        RecoveryOutcome::NoCheckpoint(_) => {
            println!("no checkpoint yet — nothing in flight to resume");
        }
        _ => println!("task unknown to Orqyn"),
    }

    Ok(())
}

#[tokio::test]
async fn readme_example_compiles_and_runs() {
    readme_example_main().await.unwrap();
}
