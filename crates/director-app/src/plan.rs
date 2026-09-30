//! The PLAN step of Orqyn's loop.
//!
//! PLAN takes the state OBSERVE produced and an objective, and turns the
//! objective into a structured plan: a set of tasks, the dependencies between
//! them, and a deterministic execution order — all persisted, all replacing
//! whatever the project was executing against before.
//!
//! ```text
//! OBSERVE → PLAN → ASSIGN
//!           ▲
//!           │
//!   current project state
//!   + an objective
//!   + a decomposition
//! ```
//!
//! ## What PLAN is responsible for, and what it is not
//!
//! PLAN owns the *shape* of the work. It validates the decomposition it is
//! given — no self-dependencies, no dependencies on tasks outside the plan, no
//! cycles — and it persists the plan and every task in one transaction, so a
//! partially created plan is impossible. It activates the plan, which is what
//! supersession is: the project's previously active plan is marked superseded,
//! linked both ways, and never deleted.
//!
//! PLAN does **not** execute tasks, does not assign agents, does not verify
//! work, and does not decide *what* the tasks should be. The decomposition —
//! which tasks, which titles, which dependencies — arrives as
//! [`TaskSpec`]s from the caller. That is deliberate: the judgment that "add
//! authentication" decomposes into model → endpoints → middleware → tests is a
//! reasoning step, and it stays outside this function so the function stays
//! deterministic and testable. Orqyn's job here is to make a stated
//! decomposition real, ordered, and durable — not to invent it.
//!
//! ## Determinism
//!
//! For identical input, [`plan`] produces identical output. Task ordering comes
//! from a topological sort whose tie-break is the order the caller listed the
//! tasks, never the clock and never hashmap iteration order. Timestamps are
//! recorded for auditability, but nothing that affects the plan's structure
//! depends on them. [`plan_at`] exists so a test can pin the clock and assert
//! that observation versions and plan times advance in order.
//!
//! ## Where decisions and blockers fit
//!
//! A [`Decision`] is a choice a project is committed to, with its own
//! supersession lifecycle; a [`Blocker`] is something standing between a task
//! and its next step. Neither is a task, and PLAN does not create either. If an
//! observed condition means an objective cannot be planned, the right response
//! is to *not plan it* — the caller decides that before calling — rather than
//! for this step to invent a blocker. When a decision constrains how a task is
//! decomposed, it does so in the caller's reasoning and lands in the plan's
//! `rationale`.

use director_domain::graph::{GraphError, PlanNode};
use director_domain::ids::{AgentId, PlanId, ProjectId, TaskId};
use director_domain::plan::Plan;
use director_domain::store::StoredProjectState;
use director_domain::task::{Complexity, ExpectedOutput, Priority, Task, TaskStatus};
use director_domain::{order_plan, PlanRepository, ProjectRepository, ProjectStateRepository};
use director_store::Store;

use crate::PlanError;

/// A request to plan an objective for a project.
///
/// Everything Orqyn needs to turn an objective into a persisted, active plan.
/// The caller names the ids: the plan's own id and every task's id. That keeps
/// planning deterministic — the same request always yields the same plan — and
/// keeps ids human-readable across a restart.
#[derive(Debug, Clone)]
pub struct PlanRequest {
    /// The project the plan is for. Must already be known to Orqyn, which in
    /// practice means [`crate::observe`] has run for it.
    pub project_id: ProjectId,
    /// The identifier the new plan will have. A plan id is unique for the life
    /// of the plan; replanning means a new plan id, not reusing an old one.
    pub plan_id: PlanId,
    /// What the plan is for, stated as an outcome: "add authentication to the
    /// API". Recorded on the plan so it stays self-explanatory after the
    /// originating prompt scrolls away.
    pub objective: String,
    /// Why the plan decomposes the way it does: the alternatives considered and
    /// rejected. Grounds the plan in observed fact rather than invention, and
    /// is where a decision that shaped the decomposition gets recorded.
    pub rationale: String,
    /// The tasks the objective decomposes into, in any order — ordering is
    /// computed from dependencies, not assumed from this list.
    pub tasks: Vec<TaskSpec>,
    /// The agent authorizing the plan, recorded on the plan when it is
    /// activated. Provenance only: this does not assign the agent to anything.
    pub authorized_by: AgentId,
}

/// One task's shape before it exists.
///
/// The planner's unit of input. A spec names its own id and the ids of the
/// tasks it depends on; Orqyn validates that graph and turns each spec into a
/// persisted [`Task`].
#[derive(Debug, Clone)]
pub struct TaskSpec {
    /// The id the task will have. Must be unique within the plan.
    pub id: TaskId,
    /// Short human-facing label.
    pub title: String,
    /// What "done" means, in the agent's terms — the acceptance contract.
    pub objective: String,
    /// Tasks that must be done before this one starts, by id. All of them must
    /// be specs in the same plan: a dependency on a task outside the plan is a
    /// premise Orqyn cannot check, so it is rejected rather than assumed.
    pub depends_on: Vec<TaskId>,
    /// Observable criteria for completion. Empty is allowed but yields a task
    /// that the future VERIFY step can never confirm; the caller should fill
    /// these in for anything it intends to treat as done.
    pub expected_outputs: Vec<ExpectedOutput>,
    /// Scheduling priority, if any.
    pub priority: Option<Priority>,
    /// Rough size, for load balancing.
    pub complexity: Complexity,
}

impl TaskSpec {
    /// A task with no dependencies, no criteria, and no sizing. The caller
    /// fills in what it knows.
    pub fn new(id: TaskId, title: impl Into<String>, objective: impl Into<String>) -> Self {
        TaskSpec {
            id,
            title: title.into(),
            objective: objective.into(),
            depends_on: vec![],
            expected_outputs: vec![],
            priority: None,
            complexity: Complexity::Unknown,
        }
    }

    /// Declare that this task depends on `dep`. Chainable.
    pub fn depends_on(mut self, dep: TaskId) -> Self {
        self.depends_on.push(dep);
        self
    }

    /// Attach an acceptance criterion. Chainable.
    pub fn expects(mut self, output: impl Into<String>) -> Self {
        self.expected_outputs.push(ExpectedOutput {
            criterion: output.into(),
            check: None,
        });
        self
    }

    /// Record the rough size of the work. Chainable.
    pub fn sized(mut self, complexity: Complexity) -> Self {
        self.complexity = complexity;
        self
    }
}

/// The outcome of a PLAN step: an active plan and the tasks it decomposes into.
///
/// The tasks are in deterministic execution order — the same order the plan
/// itself records — so a caller reads the plan and its work as one sequence.
/// This is what the future ASSIGN step will read to decide what to hand out.
#[derive(Debug, Clone)]
pub struct Planned {
    /// The plan, now active for its project. Its `supersedes` links to the
    /// plan it replaced, if any.
    pub plan: Plan,
    /// The plan's tasks in execution order.
    pub tasks: Vec<Task>,
    /// The plan this one replaced, if the project was already executing one.
    /// The superseded plan is retained for audit, not deleted.
    pub superseded: Option<PlanId>,
}

/// Run the PLAN step: turn a request into a persisted, active plan.
///
/// The plan and all its tasks are written in a single transaction, so the store
/// never holds a plan without its tasks. Activation is part of the same
/// transaction as creation, which is what makes "at most one active plan per
/// project" real rather than aspirational — if anything fails, the previous
/// plan stays active and nothing new is written.
///
/// Planning is idempotent in shape but not in identity: calling this twice with
/// the *same* [`PlanId`] is an error, because a plan's id is its identity.
/// Calling it with a new id supersedes the sitting plan, which is how replanning
/// will work when that step is built.
pub async fn plan(store: &Store, request: PlanRequest) -> Result<Planned, PlanError> {
    plan_at(store, request, chrono::Utc::now()).await
}

/// [`plan`], with the plan time supplied by the caller. Exposed so a test can
/// pin the clock and distinguish two plans; production passes
/// [`chrono::Utc::now`].
pub async fn plan_at(
    store: &Store,
    request: PlanRequest,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Planned, PlanError> {
    // 1. The project must be known. OBSERVE creates it as a side effect, so in
    //    the running loop this is already true; a caller that plans a project
    //    Orqyn has never seen is asking for work with no context to ground it.
    store
        .projects()
        .get_project(&request.project_id)
        .await
        .map_err(|err| match err {
            director_domain::StoreError::NotFound(_) => {
                PlanError::UnknownProject(request.project_id.to_string())
            }
            other => PlanError::Store(other.to_string()),
        })?;

    // 2. The request has to say something, and has to say it coherently. This is
    //    the pure half of validation — no store, no I/O — so it is also testable
    //    on its own (see [`validate_request`]).
    validate_request(&request)?;

    // A plan id is the plan's identity; reusing one would overwrite the audit
    // trail. The store would refuse this too, on the primary key — checking
    // here turns a storage error into a message a caller can act on.
    match store.plans().get_plan(&request.plan_id).await {
        Ok(_) => return Err(PlanError::DuplicatePlan(request.plan_id.to_string())),
        Err(director_domain::StoreError::NotFound(_)) => {}
        Err(other) => return Err(PlanError::Store(other.to_string())),
    }

    // 3. Validate the dependency graph and compute the execution order. This is
    //    the step's real judgment: it refuses self-dependencies, dangling
    //    dependencies, and cycles, and it orders the work deterministically.
    //    Ordering is computed before anything is persisted, so an invalid plan
    //    writes nothing at all.
    let nodes: Vec<PlanNode> = request
        .tasks
        .iter()
        .map(|spec| PlanNode::with_dependencies(spec.id.clone(), spec.depends_on.clone()))
        .collect();
    let order = order_plan(&nodes).map_err(PlanError::from)?;

    // 4. Ground the plan in what was observed. The rationale records why the
    //    decomposition looks like it does, plus the observation it was made
    //    against, so the plan stays self-explanatory after the state moves on.
    let observed = store
        .project_state()
        .get_project_state(&request.project_id)
        .await
        .map_err(PlanError::from)?;
    let rationale = grounded_rationale(&request.rationale, observed.as_ref());

    // 5. Assemble the plan. It starts as a draft and is activated inside the
    //    store transaction; task_ids is the authoritative order from step 3.
    let objective = request.objective.trim();
    let mut plan = Plan::draft(
        request.plan_id.clone(),
        request.project_id.clone(),
        objective,
        rationale,
    );
    plan.created_at = now;
    plan.updated_at = now;
    for id in &order {
        plan.add_task(id.clone());
    }

    // 6. Assemble the tasks. A task in an active plan is `Todo` — waiting on a
    //    dependency or an agent — never `Done`: nothing self-reports
    //    completion, and the VERIFY step is what moves a task there.
    let tasks: Vec<Task> = request
        .tasks
        .iter()
        .map(|spec| {
            let mut task = Task::for_project(
                request.project_id.clone(),
                spec.id.clone(),
                spec.title.trim(),
                spec.objective.trim(),
            );
            task.status = TaskStatus::Todo;
            task.dependencies = spec.depends_on.clone();
            task.expected_outputs = spec.expected_outputs.clone();
            task.priority = spec.priority;
            task.complexity = spec.complexity;
            task.created_at = now;
            task.updated_at = now;
            task
        })
        .collect();

    // 7. Persist and activate, atomically. The store supersedes the project's
    //    sitting active plan within the same transaction and returns the plan
    //    as it now stands, so `supersedes` reflects what was written.
    let (stored_plan, stored_tasks) = store
        .create_active_plan(&plan, &tasks, &request.authorized_by, now)
        .await?;

    let superseded = stored_plan.supersedes.clone();
    Ok(Planned {
        superseded,
        plan: stored_plan,
        tasks: stored_tasks,
    })
}

/// The pure half of request validation: no store, no I/O, no clock.
///
/// Checks everything about a [`PlanRequest`] that can be known without looking
/// at the world — the objective is non-empty, there is at least one task, task
/// ids are unique, and every task has both a title and an objective. Returns
/// `Ok(())` for a request whose *shape* is right; whether the project exists
/// and whether the plan id is free are store questions, and are asked in
/// [`plan_at`] after this passes.
///
/// Extracted so the shape rules can be tested without standing up a database,
/// and so a caller can reject a malformed request before it pays for any I/O.
pub fn validate_request(request: &PlanRequest) -> Result<(), PlanError> {
    // An empty objective or an empty task list is not a plan, and persisting
    // either would leave an active plan the loop could never act on.
    if request.objective.trim().is_empty() {
        return Err(PlanError::MissingObjective);
    }
    if request.tasks.is_empty() {
        return Err(PlanError::NoTasks);
    }

    // Duplicate task ids would silently collapse two pieces of work into one
    // stored task, so they are refused before anything is built.
    let mut seen = std::collections::HashSet::new();
    for spec in &request.tasks {
        if !seen.insert(spec.id.clone()) {
            return Err(PlanError::DuplicateTask(spec.id.clone()));
        }
        if spec.title.trim().is_empty() || spec.objective.trim().is_empty() {
            return Err(PlanError::InvalidTask {
                id: spec.id.clone(),
                problem: "a task needs both a title and an objective".to_string(),
            });
        }
    }

    Ok(())
}

/// Compose the plan's rationale with the observation it was made against.
///
/// Deterministic in the observation: the same observed state yields the same
/// rationale text. A short sha keeps the message readable; a repository with no
/// commits yet reports that instead of an empty string.
fn grounded_rationale(caller_rationale: &str, observed: Option<&StoredProjectState>) -> String {
    let why = caller_rationale.trim();
    match observed {
        Some(state) => {
            let head = if state.head_commit.is_empty() {
                "<no commits yet>".to_string()
            } else {
                state.head_commit.chars().take(12).collect()
            };
            let branch = state.branch.as_deref().unwrap_or("detached HEAD");
            format!(
                "{why} — decomposed from observation {} at {head} on {branch}",
                state.observation_version
            )
        }
        None => format!("{why} — planned without a prior observation of the project"),
    }
}

impl From<GraphError> for PlanError {
    fn from(err: GraphError) -> Self {
        match err {
            GraphError::SelfDependency(task) => PlanError::InvalidDependency {
                task,
                problem: "a task cannot depend on itself".to_string(),
            },
            GraphError::UnknownDependency { task, dependency } => {
                PlanError::UnknownDependency { task, dependency }
            }
            GraphError::Cycle { cycle } => PlanError::CircularDependency { cycle },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_builder_accumulates_dependencies_and_criteria() {
        let spec = TaskSpec::new(
            TaskId::from_string("AUTH-2"),
            "Login endpoint",
            "POST /login returns a session",
        )
        .depends_on(TaskId::from_string("AUTH-1"))
        .depends_on(TaskId::from_string("AUTH-3"))
        .expects("a valid user gets a session cookie")
        .sized(Complexity::Medium);

        assert_eq!(
            spec.depends_on,
            vec![TaskId::from_string("AUTH-1"), TaskId::from_string("AUTH-3")]
        );
        assert_eq!(spec.expected_outputs.len(), 1);
        assert_eq!(spec.complexity, Complexity::Medium);
        assert_eq!(spec.priority, None);
    }

    #[test]
    fn an_empty_objective_is_rejected_before_any_store_call() {
        // Shape validation is pure: it does not need a store, so it is tested
        // here at the unit level rather than through the integration suite.
        let request = PlanRequest {
            project_id: ProjectId::from_string("PROJ-1"),
            plan_id: PlanId::from_string("PLAN-1"),
            objective: "   ".to_string(),
            rationale: "why".to_string(),
            tasks: vec![TaskSpec::new(
                TaskId::from_string("AUTH-1"),
                "Model",
                "Build the model",
            )],
            authorized_by: AgentId::from_string("AGENT-planner"),
        };

        assert!(
            matches!(validate_request(&request), Err(PlanError::MissingObjective)),
            "got {:?}",
            validate_request(&request)
        );
    }

    #[test]
    fn an_empty_task_list_is_rejected() {
        let request = PlanRequest {
            project_id: ProjectId::from_string("PROJ-1"),
            plan_id: PlanId::from_string("PLAN-1"),
            objective: "add authentication".to_string(),
            rationale: "why".to_string(),
            tasks: vec![],
            authorized_by: AgentId::from_string("AGENT-planner"),
        };

        assert!(
            matches!(validate_request(&request), Err(PlanError::NoTasks)),
            "got {:?}",
            validate_request(&request)
        );
    }

    #[test]
    fn duplicate_task_ids_are_rejected() {
        let request = PlanRequest {
            project_id: ProjectId::from_string("PROJ-1"),
            plan_id: PlanId::from_string("PLAN-1"),
            objective: "add authentication".to_string(),
            rationale: "why".to_string(),
            tasks: vec![
                TaskSpec::new(TaskId::from_string("AUTH-1"), "Model", "Build the model"),
                TaskSpec::new(TaskId::from_string("AUTH-1"), "Again", "Same id"),
            ],
            authorized_by: AgentId::from_string("AGENT-planner"),
        };

        assert!(
            matches!(validate_request(&request), Err(PlanError::DuplicateTask(t)) if t == TaskId::from_string("AUTH-1")),
            "got {:?}",
            validate_request(&request)
        );
    }

    #[test]
    fn a_task_with_no_title_is_rejected() {
        let request = PlanRequest {
            project_id: ProjectId::from_string("PROJ-1"),
            plan_id: PlanId::from_string("PLAN-1"),
            objective: "add authentication".to_string(),
            rationale: "why".to_string(),
            tasks: vec![TaskSpec::new(
                TaskId::from_string("AUTH-1"),
                "   ",
                "Build the model",
            )],
            authorized_by: AgentId::from_string("AGENT-planner"),
        };

        assert!(
            matches!(
                validate_request(&request),
                Err(PlanError::InvalidTask { .. })
            ),
            "got {:?}",
            validate_request(&request)
        );
    }

    #[test]
    fn graph_errors_map_to_plan_errors() {
        let self_dep = GraphError::SelfDependency(TaskId::from_string("AUTH-1"));
        assert!(matches!(
            PlanError::from(self_dep),
            PlanError::InvalidDependency { task, .. } if task == TaskId::from_string("AUTH-1")
        ));

        let unknown = GraphError::UnknownDependency {
            task: TaskId::from_string("AUTH-1"),
            dependency: TaskId::from_string("NOPE"),
        };
        assert!(matches!(
            PlanError::from(unknown),
            PlanError::UnknownDependency { dependency, .. } if dependency == TaskId::from_string("NOPE")
        ));

        let cycle = GraphError::Cycle {
            cycle: vec!["A".into(), "B".into(), "A".into()],
        };
        assert!(matches!(
            PlanError::from(cycle),
            PlanError::CircularDependency { cycle } if cycle.len() == 3
        ));
    }

    #[test]
    fn the_rationale_is_grounded_in_the_observation() {
        let state = StoredProjectState {
            project_id: ProjectId::from_string("PROJ-1"),
            repository_id: None,
            branch: Some("main".into()),
            head_commit: "abcdef1234567890".to_string(),
            working_tree_clean: true,
            observation_version: 3,
            last_observed_at: chrono::Utc::now(),
            state_version: 2,
        };

        let text = grounded_rationale("login before sessions", Some(&state));
        assert!(text.contains("observation 3"), "got {text}");
        assert!(
            text.contains("abcdef123456"),
            "the sha is shortened: {text}"
        );
        assert!(text.contains("main"), "got {text}");
        assert!(text.contains("login before sessions"), "got {text}");
    }

    #[test]
    fn an_empty_repository_is_reported_in_the_rationale() {
        let state = StoredProjectState {
            project_id: ProjectId::from_string("PROJ-1"),
            repository_id: None,
            branch: None,
            head_commit: String::new(),
            working_tree_clean: true,
            observation_version: 1,
            last_observed_at: chrono::Utc::now(),
            state_version: 1,
        };

        let text = grounded_rationale("start from nothing", Some(&state));
        assert!(text.contains("<no commits yet>"), "got {text}");
        assert!(text.contains("detached HEAD"), "got {text}");
    }

    #[test]
    fn a_missing_observation_is_noted_in_the_rationale() {
        let text = grounded_rationale("no observation yet", None);
        assert!(text.contains("without a prior observation"), "got {text}");
    }

    #[test]
    fn validate_request_catches_shape_errors_purely() {
        let mut request = PlanRequest {
            project_id: ProjectId::from_string("PROJ-1"),
            plan_id: PlanId::from_string("PLAN-1"),
            objective: "add authentication".to_string(),
            rationale: "why".to_string(),
            tasks: vec![TaskSpec::new(
                TaskId::from_string("AUTH-1"),
                "Model",
                "Build the model",
            )],
            authorized_by: AgentId::from_string("AGENT-planner"),
        };
        assert!(validate_request(&request).is_ok());

        request.objective = "  ".into();
        assert!(matches!(
            validate_request(&request),
            Err(PlanError::MissingObjective)
        ));
        request.objective = "add authentication".into();

        request.tasks.clear();
        assert!(matches!(
            validate_request(&request),
            Err(PlanError::NoTasks)
        ));
        request.tasks.push(TaskSpec::new(
            TaskId::from_string("AUTH-1"),
            "Model",
            "Build the model",
        ));

        request.tasks.push(TaskSpec::new(
            TaskId::from_string("AUTH-1"),
            "Dupe",
            "Duplicate id",
        ));
        assert!(
            matches!(validate_request(&request), Err(PlanError::DuplicateTask(t)) if t == TaskId::from_string("AUTH-1")),
            "got {:?}",
            validate_request(&request)
        );
        request.tasks.pop();

        request.tasks[0].title = "  ".into();
        assert!(matches!(
            validate_request(&request),
            Err(PlanError::InvalidTask { .. })
        ));
    }
}
