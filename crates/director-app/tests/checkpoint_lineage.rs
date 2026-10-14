use std::process::Command;

use director_adapters::git::GitService;
use director_adapters::InMemoryProvider;
use director_app::observe::observe;
use director_app::plan::{plan, PlanRequest, TaskSpec};
use director_domain::ids::{AgentId, IdGenerator, PlanId, ProjectId, RepositoryId, TaskId};
use director_domain::project::Project;
use director_domain::{AgentRepository, CheckpointRepository, ProjectRepository};
use director_store::Store;

struct Scr {
    dir: std::path::PathBuf,
}

impl Scr {
    fn new() -> Self {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let setups: [&[&str]; 4] = [
            &["init", "-q"],
            &["config", "user.name", "t"],
            &["config", "user.email", "t@t"],
            &["config", "commit.gpgsign", "false"],
        ];
        for args in setups {
            Command::new("git")
                .current_dir(&repo)
                .args(args)
                .status()
                .unwrap();
        }
        std::fs::write(repo.join("README.md"), "hi\n").unwrap();
        Command::new("git")
            .current_dir(&repo)
            .args(["add", "README.md"])
            .status()
            .unwrap();
        Command::new("git")
            .current_dir(&repo)
            .args(["commit", "-q", "-m", "init"])
            .status()
            .unwrap();
        std::mem::forget(tmp);
        std::fs::create_dir_all(&dir).unwrap();
        Scr { dir }
    }

    fn commit(&self, p: &str, c: &str) {
        let repo = self.dir.join("repo");
        let file = repo.join(p);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&file, c).unwrap();
        Command::new("git")
            .current_dir(&repo)
            .args(["add", p])
            .status()
            .unwrap();
        Command::new("git")
            .current_dir(&repo)
            .args(["commit", "-q", "-m", "x"])
            .status()
            .unwrap();
    }

    fn path(&self) -> String {
        self.dir.join("repo").to_string_lossy().replace('\\', "/")
    }
}

/// The regression that motivated the lineage-derived id: a restarted process
/// holds a fresh `IdGenerator` whose counters are back at zero, and the old
/// code re-minted `CHK-work-1` for a task that already had it, colliding on
/// `checkpoints.id`'s PRIMARY KEY and failing the whole observation round.
#[tokio::test]
async fn a_restart_advances_the_lineage_instead_of_colliding() {
    let scr = Scr::new();
    let store = Store::open(scr.dir.join("orqyn.db")).await.unwrap();
    std::fs::create_dir_all(scr.dir.join("gitstate")).unwrap();
    let git = GitService::new(scr.dir.join("gitstate")).expect("git service");
    let project = ProjectId::from_string("PROJ-1");
    let repo = RepositoryId::from_string("REPO-1");
    let path = scr.path();
    let task_id = TaskId::from_string("WORK");

    store
        .projects()
        .create_project(&Project::new(project.clone(), project.as_str(), &path))
        .await
        .unwrap();
    let mut ids = IdGenerator::new();
    plan(
        &store,
        PlanRequest {
            project_id: project.clone(),
            plan_id: PlanId::from_string("PLAN-1"),
            objective: "o".into(),
            rationale: "r".into(),
            tasks: vec![TaskSpec::new(task_id.clone(), "WORK", "do the work")],
            authorized_by: AgentId::from_string("AGENT-1"),
        },
    )
    .await
    .unwrap();
    store
        .agents()
        .register_agent(&director_domain::agent::Agent::register(
            AgentId::from_string("AGENT-1"),
            "cc".to_string(),
            director_domain::agent::Harness::ClaudeCode,
            director_domain::ids::MachineId::from_string("M-1".to_string()),
            vec![director_domain::capability::Capability::Coding],
        ))
        .await
        .unwrap();
    director_app::schedule::schedule(&store, &mut ids, &project)
        .await
        .unwrap();

    // First real change: CHK-work-1. (The round before this one was the
    // registration baseline, which checkpoints nothing.)
    scr.commit("src/b.rs", "b\n");
    observe(
        &store,
        &git,
        &mut IdGenerator::new(),
        &InMemoryProvider::new(),
        &project,
        &repo,
        &path,
    )
    .await
    .unwrap();

    // Second real change: CHK-work-2, still in the "same process".
    scr.commit("src/c.rs", "c\n");
    observe(
        &store,
        &git,
        &mut IdGenerator::new(),
        &InMemoryProvider::new(),
        &project,
        &repo,
        &path,
    )
    .await
    .unwrap();

    // Simulated restart: fresh generator, counters at zero. Before the fix this
    // re-minted CHK-work-1 — the id a previous process already used — and the
    // round errored on `UNIQUE constraint failed: checkpoints.id`.
    scr.commit("src/d.rs", "d\n");
    let round2 = observe(
        &store,
        &git,
        &mut IdGenerator::new(),
        &InMemoryProvider::new(),
        &project,
        &repo,
        &path,
    )
    .await
    .expect("the restarted round advances the lineage");

    let lineage = store
        .checkpoints()
        .checkpoints_for_task(&task_id)
        .await
        .unwrap();
    let ids: Vec<_> = lineage.iter().map(|c| c.id.as_str().to_string()).collect();
    println!("lineage after restart: {ids:?}");
    println!(
        "round2 wrote: {:?}",
        round2
            .checkpoints
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>()
    );

    assert_eq!(
        ids,
        vec!["CHK-work-2", "CHK-work-1"],
        "the lineage advanced across the restart instead of colliding"
    );
}
