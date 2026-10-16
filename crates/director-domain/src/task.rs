//! The [Task] — Orqyn's central unit of work.
//!
//! ## The fundamental invariant
//!
//! A `Task` **has no agent field**. Assignment is a separate, first-class
//! entity ([`crate::assignment::AgentAssignment`]) that links a task to an
//! agent for a period. This is not stylistic: it is what makes
//!
//! > TASK-42: Claude works on it, then Codex, then DeepSeek, then a human.
//!
//! an ordinary reassignment rather than a data migration. A task's identity,
//! objective, dependencies, and history all outlive every agent that touches
//! it, and the compiler refuses to let an agent id reach a task's fields.
//!
//! Compare this with handoff-mcp's `TaskData`, which carries `assignee` and a
//! `lock` on the record itself — fine for a single-project substrate, wrong
//! for Orqyn, where the assignment *history* is part of the value.

use serde::{Deserialize, Serialize};

use crate::capability::Capability;
use crate::ids::{ProjectId, SubtaskId, TaskId};

/// Scheduling priority. Lower `rank()` sorts earlier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Must be done before anything else.
    Critical,
    /// Important, but not blocking.
    High,
    /// Normal priority.
    Medium,
    /// Can wait.
    Low,
}

impl Priority {
    /// Sort order: Critical first. `None` (no priority set) sorts last, which
    /// is why this returns `u32::MAX` for the absent case via the caller.
    pub const fn rank(self) -> u32 {
        match self {
            Priority::Critical => 0,
            Priority::High => 1,
            Priority::Medium => 2,
            Priority::Low => 3,
        }
    }
}

impl std::fmt::Display for Priority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Priority::Critical => "critical",
            Priority::High => "high",
            Priority::Medium => "medium",
            Priority::Low => "low",
        };
        f.write_str(s)
    }
}

/// Rough complexity, used for load balancing and estimate-based scheduling.
///
/// Orqyn deliberately does not emit hour estimates at plan time: plans are
/// grounded in observed state, and an invented "this will take 4 hours" is a
/// fact the planner cannot actually check. `Complexity` is a coarse ordinal
/// that a planner can justify from the task's surface area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Complexity {
    /// A one-line change.
    Trivial,
    /// A small, contained change.
    Small,
    /// A multi-file change.
    Medium,
    /// A substantial change touching many files.
    Large,
    /// Not yet estimated.
    Unknown,
}

impl Complexity {
    /// Ordinal for load balancing. `Unknown` sorts heaviest so an unestimated
    /// task is not silently preferred over a sized one.
    pub const fn weight(self) -> u32 {
        match self {
            Complexity::Trivial => 1,
            Complexity::Small => 2,
            Complexity::Medium => 4,
            Complexity::Large => 8,
            Complexity::Unknown => 16,
        }
    }
}

/// Lifecycle state of a task.
///
/// Transitions are driven by Orqyn, not by the agent working on it. In
/// particular, an agent reporting "done" moves a task to
/// [`TaskStatus::VerificationPending`], **never** to [`TaskStatus::Done`] —
/// only the verification engine (Phase 10) can do that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Known to Orqyn, not yet planned into an active plan.
    Backlog,
    /// In an active plan, waiting for a dependency or an agent.
    Todo,
    /// An agent is assigned and expected to be working.
    InProgress,
    /// Blocked on a [Blocker](crate::blocker::Blocker).
    Blocked,
    /// Agent claims completion; awaiting independent verification.
    VerificationPending,
    /// Verification passed against the task's expected output.
    Done,
    /// Verification failed. The task needs rework, possibly by another agent.
    Failed,
    /// Superseded or deliberately abandoned by the planner.
    Cancelled,
}

impl TaskStatus {
    /// True if the task still requires work of some kind.
    pub fn is_open(self) -> bool {
        matches!(
            self,
            TaskStatus::Backlog
                | TaskStatus::Todo
                | TaskStatus::InProgress
                | TaskStatus::Blocked
                | TaskStatus::VerificationPending
        )
    }

    /// True if the task is finished, one way or another.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }

    /// True if an agent's report can put the task into this state. `Done` is
    /// deliberately absent — this is the guardrail behind the acceptance test
    /// "agent claims done, tests fail → must not become COMPLETED".
    pub fn reachable_by_agent_report(self) -> bool {
        matches!(
            self,
            TaskStatus::InProgress | TaskStatus::Blocked | TaskStatus::VerificationPending
        )
    }
}

/// A requirement that must be observably true for the task to be done.
///
/// Textual, because it is what the planner produces from the objective. The
/// verification engine (Phase 10) is what turns these into checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedOutput {
    /// Human-readable acceptance criterion, e.g. "POST /login returns 200 with
    /// a session cookie for a valid user".
    pub criterion: String,
    /// Optional machine-checkable form, e.g. a test name or a command.
    pub check: Option<String>,
}

/// A unit of work.
///
/// Note what is absent: no `assignee`, no `agent_id`, no lock. Those live on
/// [`crate::assignment::AgentAssignment`] and on the substrate's own lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// Stable, agent-independent identifier.
    pub id: TaskId,
    /// The project this task belongs to. `None` for a task Orqyn has seen in
    /// a substrate but not yet claimed for a project; Orqyn's own store
    /// requires one at write time and rejects the alternative rather than
    /// inventing a project.
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    /// Short human-facing label.
    pub title: String,
    /// What "done" means, in the agent's terms.
    pub objective: String,
    /// Longer prose: context, rationale, links. Distinct from `objective`,
    /// which is the acceptance contract; this is the narrative around it.
    #[serde(default)]
    pub description: Option<String>,
    /// Observable criteria for completion. Empty is legal but produces a task
    /// that can never be verified — the planner must fill this in.
    pub expected_outputs: Vec<ExpectedOutput>,
    /// Lifecycle state.
    pub status: TaskStatus,
    /// Scheduling priority, if set.
    pub priority: Option<Priority>,
    /// Rough size, for load balancing.
    pub complexity: Complexity,
    /// Tasks that must reach a terminal state before this one starts.
    pub dependencies: Vec<TaskId>,
    /// What this task may touch, used for deterministic conflict detection.
    /// Advisory for the agent, authoritative for Orqyn.
    ///
    /// **An empty list means the scope is undeclared, not that the task is
    /// safe to run beside anything.** See [`Task::scope_overlap`] and
    /// [`normalize_scope_path`] for the full semantics, and note that an
    /// undeclared scope overlaps *everything*, including another undeclared
    /// scope. Declare scopes with [`crate::task::Task::overlaps_scope`]'s
    /// callers — [`crate::assign`] and [`crate::schedule`] — or, when planning,
    /// with the `scopes` builder on the planner's task spec.
    pub scope_paths: Vec<String>,
    /// Capabilities an agent must declare to be assigned this task.
    pub required_capabilities: Vec<Capability>,
    /// Decomposition for larger tasks.
    pub subtasks: Vec<Subtask>,
    /// Monotonic version for optimistic concurrency — see
    /// [`crate::project::Project::state_version`]. Bumped by the store on a
    /// successful update; two writers who both read version *n* cannot both
    /// write *n+1*.
    #[serde(default = "default_state_version")]
    pub state_version: u64,
    /// When Orqyn first recorded the task.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When the task last changed.
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// The version a freshly persisted record starts on. One, not zero: a record
/// that exists has been written once. Shared with
/// [`crate::project::Project::state_version`] so every mutable entity starts
/// from the same baseline.
fn default_state_version() -> u64 {
    1
}

impl Task {
    /// Create a new task in `Backlog` with no dependencies or subtasks.
    pub fn new(id: TaskId, title: impl Into<String>, objective: impl Into<String>) -> Self {
        let now = chrono::Utc::now();
        Task {
            id,
            project_id: None,
            title: title.into(),
            objective: objective.into(),
            description: None,
            expected_outputs: vec![],
            status: TaskStatus::Backlog,
            priority: None,
            complexity: Complexity::Unknown,
            dependencies: vec![],
            scope_paths: vec![],
            required_capabilities: vec![],
            subtasks: vec![],
            state_version: default_state_version(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Create a task that belongs to `project_id`. The shape the store expects;
    /// [`Task::new`] leaves the project unset for callers that only need an
    /// unconstrained task.
    pub fn for_project(
        project_id: ProjectId,
        id: TaskId,
        title: impl Into<String>,
        objective: impl Into<String>,
    ) -> Self {
        let mut task = Task::new(id, title, objective);
        task.project_id = Some(project_id);
        task
    }

    /// Whether every dependency has completed successfully.
    ///
    /// Used by the scheduler. This is deliberately a pure function over the
    /// statuses passed in: Orqyn never trusts a stored "ready" flag.
    ///
    /// A dependency only satisfies this when it is [`TaskStatus::Done`] — a
    /// `Failed` or `Cancelled` dependency does **not** unblock the task. That
    /// is the point where the planner's dependency walk must notice the
    /// failure and replan, rather than silently proceeding onto work whose
    /// premise no longer holds.
    pub fn is_ready_given<'a, I>(&self, status_of: I) -> bool
    where
        I: Fn(&TaskId) -> Option<TaskStatus> + 'a,
    {
        self.dependencies
            .iter()
            .all(|dep| matches!(status_of(dep), Some(TaskStatus::Done)))
    }

    /// True if this task's scope overlaps another task's scope.
    ///
    /// A thin wrapper over [`Task::scope_overlap`] for callers that only need
    /// the boolean. Conflict *detection* in production wants the
    /// [`ScopeOverlap`], because "these two tasks conflict" is not actionable
    /// and "these two tasks conflict over `src/auth`" is.
    pub fn overlaps_scope(&self, other: &Task) -> bool {
        self.scope_overlap(other).is_some()
    }

    /// Where this task's declared scope collides with another task's, or `None`
    /// when the two can safely be in flight at the same time.
    ///
    /// ## What "overlap" means here
    ///
    /// Two declared scopes overlap when one is a **path prefix** of the other,
    /// after normalization. That single rule covers the cases that matter:
    ///
    /// - identical scopes — equal, so each is a prefix of the other;
    /// - a parent directory and something inside it — `src` vs `src/auth`;
    /// - the same file named twice — `src/auth/login.rs` against itself;
    /// - the repository root — `/` normalizes to no components at all, and the
    ///   empty sequence is a prefix of everything, so a root scope covers the
    ///   whole tree.
    ///
    /// Overlap is deliberately **conservative**: a directory scope conflicts
    /// with work inside it even when the two touch disjoint files. Reporting a
    /// conflict that turns out to be harmless costs one scheduling round;
    /// missing a real one costs a corrupted merge. Semantic conflict detection
    /// — reading the files and reasoning about them — is explicitly out of
    /// scope for this version.
    ///
    /// ## An undeclared scope is not an empty scope
    ///
    /// A task whose `scope_paths` is empty, or whose entries are all unusable,
    /// has an **undeclared** scope: Orqyn does not know what it may touch.
    /// That is treated as overlapping *everything*, including another
    /// undeclared scope. The alternative — reading an empty list as "no
    /// constraint, safe to run beside anything" — would make the default
    /// construction of [`Task::new`] silently unsafe, which is exactly the
    /// failure this check exists to prevent.
    ///
    /// The cost is that a caller who declares no scopes gets no concurrency at
    /// all, which is the honest consequence of not saying what the work may
    /// touch. Declaring disjoint scopes is what buys parallelism back.
    ///
    /// ## Determinism across platforms
    ///
    /// Comparison is case-sensitive and separator-normalized **on every
    /// platform**. Orqynn is deciding whether two *declared* scopes cover the
    /// same ground; it is not asking the filesystem. Lower-casing only on
    /// Windows and macOS would make the same plan produce different assignments
    /// on different machines, so the rule does not vary. See the module
    /// documentation for what that costs on a case-insensitive filesystem.
    pub fn scope_overlap(&self, other: &Task) -> Option<ScopeOverlap> {
        if self.id == other.id {
            return None;
        }

        let mine = normalize_scope_paths(&self.scope_paths);
        let theirs = normalize_scope_paths(&other.scope_paths);

        // Undeclared means "anything". Either side being undeclared is enough to
        // collide, so the collision cannot always be located at named paths.
        let mut paths = Vec::new();
        if let (Some(mine), Some(theirs)) = (&mine, &theirs) {
            for a in mine {
                for b in theirs {
                    if scopes_overlap(a, b) {
                        paths.push(ScopeOverlapPaths {
                            declared_in_self: self.declared_scope_matching(a),
                            declared_in_other: other.declared_scope_matching(b),
                            normalized: a.join("/"),
                        });
                    }
                }
            }
            if paths.is_empty() {
                return None;
            }
            return Some(ScopeOverlap {
                paths,
                undeclared: false,
            });
        }

        // At least one side declared nothing usable. That is an overlap with
        // everything, including the case where both sides are undeclared.
        Some(ScopeOverlap {
            paths,
            undeclared: true,
        })
    }

    /// The scope entry that normalizes to `normalized`, as this task declared
    /// it — the first such entry, so a message quotes one real declaration
    /// rather than a synthesized one the caller cannot find in their own plan.
    fn declared_scope_matching(&self, normalized: &[String]) -> String {
        self.scope_paths
            .iter()
            .find(|raw| normalize_scope_path(raw).as_deref() == Some(normalized))
            .cloned()
            .unwrap_or_else(|| normalized.join("/"))
    }

    /// Record a change, stamping `updated_at`.
    pub fn touch(&mut self) {
        self.updated_at = chrono::Utc::now();
    }

    /// Advance the optimistic-concurrency version and stamp the time. The store
    /// calls this on a successful update; callers never bump by hand.
    pub fn bump_state_version(&mut self) {
        self.state_version += 1;
        self.touch();
    }
}

/// Where two tasks' declared scopes collide.
///
/// Returned by [`Task::scope_overlap`] so a refusal can name the ground in
/// dispute rather than only the fact of a dispute. A caller that wants to
/// resolve the conflict — by splitting a scope, by sequencing the tasks with a
/// dependency, or by declaring the overlap intended — needs to know *which*
/// paths collided, not merely that two ids did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeOverlap {
    /// The colliding pairs of declarations, one entry per colliding pair. Empty
    /// when [`Self::undeclared`] is set and the collision could not be located,
    /// because an undeclared scope names no ground to point at.
    pub paths: Vec<ScopeOverlapPaths>,
    /// True when at least one of the two tasks declared no usable scope, so the
    /// overlap is total rather than located at named paths. There is nothing to
    /// narrow: declaring disjoint scopes is the only way to get concurrency back.
    pub undeclared: bool,
}

impl ScopeOverlap {
    /// A one-line description suitable for an error message.
    pub fn describe(&self) -> String {
        if self.paths.is_empty() {
            return "one of the tasks declares no scope, so it may touch anything".to_string();
        }
        let mut seen: Vec<String> = Vec::new();
        for pair in &self.paths {
            let line = format!("{} <-> {}", pair.declared_in_self, pair.declared_in_other);
            if !seen.contains(&line) {
                seen.push(line);
            }
        }
        seen.join("; ")
    }
}

/// One pair of declarations that collide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeOverlapPaths {
    /// The path as the first task declared it.
    pub declared_in_self: String,
    /// The path as the second task declared it.
    pub declared_in_other: String,
    /// The shared normalized form, for a log line or a diff.
    pub normalized: String,
}

/// Normalize every entry a task declared, or `None` if it declared nothing
/// usable.
///
/// "Nothing usable" covers an empty list, a list of blank strings, and a list
/// whose entries are all rejected by [`normalize_scope_path`]. All three mean
/// the same thing to Orqyn: it does not know what this task may touch.
pub fn normalize_scope_paths(paths: &[String]) -> Option<Vec<Vec<String>>> {
    let normalized: Vec<Vec<String>> = paths
        .iter()
        .filter_map(|raw| normalize_scope_path(raw))
        .collect();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

/// Reduce one declared scope path to the components Orqynn compares.
///
/// Returns the path split on `/` with empty, `.`, and `..` segments resolved,
/// or `None` when the declaration cannot be reduced to a usable scope.
///
/// ## The rules, and why each one is here
///
/// - **Blank is rejected.** A whitespace-only entry names nothing.
/// - **Control characters are rejected.** A NUL or a newline in a scope is not
///   a path; it is something that arrived from outside. Rejecting is cheaper
///   than guessing what it meant, and it keeps an untrusted scope out of a log
///   line that does not terminate where the author expected.
/// - **`\` becomes `/`.** The same tree is written with either separator, and
///   Orqynn is developed on Windows. Without this, `src\auth` and `src/auth`
///   would read as disjoint trees and the check would miss them.
/// - **Empty segments are dropped.** `a//b` and `a/b` are the same path.
/// - **`.` is dropped and `..` pops.** `a/./b` is `a/b`, and `a/b/../c` is
///   `a/c`. Leaving `..` unresolved would let two declarations of one directory
///   disagree, which is the bug this normalization exists to remove.
/// - **`..` cannot escape a relative path.** A relative declaration that pops
///   past its own start (`../src`) is rejected rather than clamped: it names a
///   location *outside* the project, which is not ground Orqynn is managing.
///   An absolute path clamps instead, because `/..` is `/` by definition.
/// - **A leading `/` is not carried into the comparison.** `/src/auth` and
///   `src/auth` reduce to the same components and therefore overlap. That is
///   conservative on purpose: a task declaring an absolute path and one
///   declaring a relative path may well be describing the same files, and
///   reporting a conflict costs a round while missing one costs a merge.
/// - **Case is preserved.** Comparison is case-sensitive everywhere, so the same
///   plan yields the same assignments on Linux, macOS, and Windows.
///
/// A path that normalizes to zero components is the repository root, and is
/// deliberately *not* `None`: an empty component list is a prefix of everything,
/// which is exactly what `/` should mean.
pub fn normalize_scope_path(raw: &str) -> Option<Vec<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return None;
    }

    let unified = trimmed.replace('\\', "/");
    let absolute = unified.starts_with('/');

    let mut components: Vec<String> = Vec::new();
    for segment in unified.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if components.pop().is_none() && !absolute {
                    // `..` past the start of a relative path: outside the tree.
                    return None;
                }
            }
            other => components.push(other.to_string()),
        }
    }

    Some(components)
}

/// Whether two normalized scopes cover the same ground.
///
/// The empty sequence — the repository root — is a prefix of everything, so a
/// root scope collides with any other scope. Otherwise one must be a prefix of
/// the other, which covers exact matches, parent/child directories, and a file
/// named identically by two tasks.
fn scopes_overlap(a: &[String], b: &[String]) -> bool {
    let shared = a.len().min(b.len());
    a[..shared] == b[..shared]
}

/// A subdivision of a [Task].
///
/// Subtasks share the parent's identity space: they are addressed as
/// `AUTH-42.1` and do not have their own dependencies or capabilities — those
/// come from the parent. If a piece of work needs its own dependencies, it is
/// a [Task], not a subtask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subtask {
    /// This subtask's identifier.
    pub id: SubtaskId,
    /// The task it belongs to.
    pub parent: TaskId,
    /// Short label.
    pub title: String,
    /// Whether the subtask is complete.
    pub done: bool,
    /// Position among siblings.
    pub order: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_task_has_no_agent() {
        let t = Task::new(TaskId::from_string("AUTH-42"), "Auth", "Build login");
        // The point of the exercise: there is nowhere on a Task to put an agent.
        assert_eq!(t.status, TaskStatus::Backlog);
        assert!(t.dependencies.is_empty());
        assert!(t.subtasks.is_empty());
    }

    #[test]
    fn done_is_not_reachable_by_agent_report() {
        // The acceptance criterion: an agent saying "done" must not complete
        // a task. Only verification does.
        assert!(!TaskStatus::Done.reachable_by_agent_report());
        assert!(TaskStatus::VerificationPending.reachable_by_agent_report());
    }

    #[test]
    fn open_and_terminal_partition_the_lifecycle() {
        assert!(TaskStatus::Todo.is_open());
        assert!(!TaskStatus::Todo.is_terminal());
        assert!(TaskStatus::Done.is_terminal());
        assert!(!TaskStatus::Done.is_open());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
    }

    #[test]
    fn readiness_follows_terminal_dependencies() {
        let mut t = Task::new(TaskId::from_string("AUTH-42"), "Auth", "Build login");
        t.dependencies = vec![TaskId::from_string("DB-1"), TaskId::from_string("DB-2")];

        let mut statuses = std::collections::HashMap::new();
        assert!(!t.is_ready_given(|id| statuses.get(id).copied()));

        statuses.insert(TaskId::from_string("DB-1"), TaskStatus::Done);
        assert!(!t.is_ready_given(|id| statuses.get(id).copied()));

        statuses.insert(TaskId::from_string("DB-2"), TaskStatus::Done);
        assert!(t.is_ready_given(|id| statuses.get(id).copied()));
    }

    #[test]
    fn a_failed_dependency_blocks_readiness() {
        let mut t = Task::new(TaskId::from_string("AUTH-42"), "Auth", "Build login");
        t.dependencies = vec![TaskId::from_string("DB-1")];

        let failed: std::collections::HashMap<TaskId, TaskStatus> =
            [(TaskId::from_string("DB-1"), TaskStatus::Failed)]
                .into_iter()
                .collect();
        // A failed dependency is terminal but not Done, so the task stays
        // blocked. The planner must detect this and replan.
        assert!(!t.is_ready_given(|id| failed.get(id).copied()));

        let done: std::collections::HashMap<TaskId, TaskStatus> =
            [(TaskId::from_string("DB-1"), TaskStatus::Done)]
                .into_iter()
                .collect();
        assert!(t.is_ready_given(|id| done.get(id).copied()));
    }

    #[test]
    fn an_unknown_dependency_blocks_readiness() {
        // A dependency Orqyn has no status for is treated as unresolved:
        // better to wait than to start work on an unverified premise.
        let mut t = Task::new(TaskId::from_string("AUTH-42"), "Auth", "Build login");
        t.dependencies = vec![TaskId::from_string("MISSING-1")];
        assert!(!t.is_ready_given(|_| None));
    }

    #[test]
    fn scope_overlap_is_symmetric_and_path_aware() {
        let mut a = Task::new(TaskId::from_string("A"), "A", "A");
        let mut b = Task::new(TaskId::from_string("B"), "B", "B");
        a.scope_paths = vec!["src/auth/".into()];
        b.scope_paths = vec!["src/auth/login.rs".into()];

        assert!(a.overlaps_scope(&b));
        assert!(b.overlaps_scope(&a));
    }

    #[test]
    fn disjoint_scopes_do_not_overlap() {
        let mut a = Task::new(TaskId::from_string("A"), "A", "A");
        let mut b = Task::new(TaskId::from_string("B"), "B", "B");
        a.scope_paths = vec!["src/auth/".into()];
        b.scope_paths = vec!["src/db/".into()];

        assert!(!a.overlaps_scope(&b));
    }

    #[test]
    fn a_task_does_not_overlap_itself() {
        let mut a = Task::new(TaskId::from_string("A"), "A", "A");
        a.scope_paths = vec!["src/".into()];
        assert!(!a.overlaps_scope(&a));
    }

    #[test]
    fn priority_and_complexity_order_sensibly() {
        assert!(Priority::Critical.rank() < Priority::Low.rank());
        assert!(Complexity::Trivial.weight() < Complexity::Large.weight());
        assert_eq!(Complexity::Unknown.weight(), 16);
    }

    #[test]
    fn task_round_trips_through_serde() {
        let mut t = Task::new(TaskId::from_string("AUTH-42"), "Auth", "Build login");
        t.dependencies = vec![TaskId::from_string("DB-1")];
        t.scope_paths = vec!["src/auth/".into()];
        t.expected_outputs.push(ExpectedOutput {
            criterion: "login works".into(),
            check: Some("cargo test login".into()),
        });
        t.subtasks.push(Subtask {
            id: SubtaskId::from_string("AUTH-42.1"),
            parent: t.id.clone(),
            title: "login endpoint".into(),
            done: false,
            order: 1,
        });

        let json = serde_json::to_string(&t).unwrap();
        let back: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn a_new_task_starts_at_version_one_with_no_project() {
        let t = Task::new(TaskId::from_string("AUTH-42"), "Auth", "Build login");
        assert_eq!(t.state_version, 1);
        assert!(t.project_id.is_none());
        assert!(t.description.is_none());
    }

    #[test]
    fn a_task_can_be_scoped_to_a_project() {
        let t = Task::for_project(
            ProjectId::from_string("PROJ-1"),
            TaskId::from_string("AUTH-42"),
            "Auth",
            "Build login",
        );
        assert_eq!(t.project_id.as_ref().map(|id| id.as_str()), Some("PROJ-1"));
    }

    #[test]
    fn bumping_the_task_version_advances_it_by_one() {
        let mut t = Task::new(TaskId::from_string("AUTH-42"), "Auth", "Build login");
        t.bump_state_version();
        t.bump_state_version();
        assert_eq!(t.state_version, 3);
    }

    #[test]
    fn a_pre_phase4_task_payload_still_deserializes() {
        // A payload written before Phase 4 has none of the new fields. All three
        // default, so an old task record still reads.
        let legacy = serde_json::json!({
            "id": "AUTH-42",
            "title": "Auth",
            "objective": "Build login",
            "expected_outputs": [],
            "status": "backlog",
            "priority": null,
            "complexity": "unknown",
            "dependencies": [],
            "scope_paths": [],
            "required_capabilities": [],
            "subtasks": [],
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        });
        let parsed: Task = serde_json::from_value(legacy).unwrap();
        assert_eq!(parsed.state_version, 1);
        assert!(parsed.project_id.is_none());
        assert!(parsed.description.is_none());
    }

    // ---- Phase 13: scope normalization and overlap semantics ----------------
    //
    // Every rule `normalize_scope_path` documents has a test here, and every
    // overlap case the production check depends on has one too. The old
    // implementation got several of these wrong, which is why they are pinned.

    fn parts(raw: &str) -> Option<Vec<String>> {
        normalize_scope_path(raw)
    }

    fn owned(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    /// A task with the given declared scope, for overlap assertions.
    fn scoped(id: &str, scope: &[&str]) -> Task {
        let mut t = Task::new(TaskId::from_string(id), id, id);
        t.scope_paths = owned(scope);
        t
    }

    #[test]
    fn identical_scopes_overlap() {
        let a = scoped("A", &["src/auth/"]);
        let b = scoped("B", &["src/auth/"]);
        let overlap = a.scope_overlap(&b).expect("identical scopes collide");
        assert!(!overlap.undeclared);
        assert_eq!(overlap.paths.len(), 1);
        assert_eq!(overlap.paths[0].declared_in_self, "src/auth/");
        assert_eq!(overlap.paths[0].declared_in_other, "src/auth/");
        assert_eq!(overlap.paths[0].normalized, "src/auth");
    }

    #[test]
    fn a_parent_directory_overlaps_what_is_inside_it() {
        let parent = scoped("A", &["src"]);
        let child = scoped("B", &["src/auth/login.rs"]);
        assert!(parent.overlaps_scope(&child));
        assert!(child.overlaps_scope(&parent));
    }

    #[test]
    fn different_non_overlapping_directories_do_not_overlap() {
        let a = scoped("A", &["src/auth/"]);
        let b = scoped("B", &["src/db/"]);
        assert!(!a.overlaps_scope(&b));
    }

    #[test]
    fn a_shared_prefix_that_is_not_a_directory_boundary_is_not_an_overlap() {
        // `src/auth` and `src/author` share the string prefix "src/auth" but
        // are different directories. A plain `starts_with` would get this wrong.
        let a = scoped("A", &["src/auth"]);
        let b = scoped("B", &["src/author"]);
        assert!(
            !a.overlaps_scope(&b),
            "a string prefix is not a path prefix"
        );
    }

    #[test]
    fn the_same_file_named_by_two_tasks_overlaps() {
        let a = scoped("A", &["src/auth/login.rs"]);
        let b = scoped("B", &["src/auth/login.rs"]);
        assert!(a.overlaps_scope(&b));
    }

    #[test]
    fn the_root_scope_overlaps_everything_including_another_root() {
        let root = scoped("A", &["/"]);
        let deep = scoped("B", &["src/auth/login.rs"]);
        let other_root = scoped("C", &["/"]);

        let hit = root.scope_overlap(&deep).expect("root covers the tree");
        assert_eq!(hit.paths[0].normalized, "");
        assert!(root.overlaps_scope(&other_root));
        // Symmetric: the deep task overlaps the root too.
        assert!(deep.overlaps_scope(&root));
    }

    #[test]
    fn an_undeclared_scope_is_not_safely_runnable_beside_anything() {
        // The rule the whole phase rests on: an empty scope is unknown, and
        // unknown collides. Reading it as "no constraint" would make every
        // default-constructed Task silently unsafe.
        let undeclared = scoped("A", &[]);
        let declared = scoped("B", &["src/auth/"]);

        let hit = undeclared
            .scope_overlap(&declared)
            .expect("an undeclared scope may touch anything");
        assert!(hit.undeclared, "the overlap is total, not located");
        assert!(hit.paths.is_empty(), "an undeclared scope names no ground");
        assert!(declared.overlaps_scope(&undeclared), "and it is symmetric");
    }

    #[test]
    fn two_undeclared_scopes_collide_with_each_other() {
        let a = scoped("A", &[]);
        let b = scoped("B", &[]);
        assert!(a.overlaps_scope(&b));
    }

    #[test]
    fn a_scope_made_only_of_unusable_entries_counts_as_undeclared() {
        let a = scoped("A", &["", "   "]);
        let b = scoped("B", &["src/auth/"]);
        assert!(a.overlaps_scope(&b), "blank entries declare nothing");
    }

    #[test]
    fn windows_and_unix_separators_describe_the_same_scope() {
        let unix = scoped("A", &["src/auth/login.rs"]);
        let windows = scoped("B", &[r"src\auth\login.rs"]);
        let hit = unix
            .scope_overlap(&windows)
            .expect("one tree, two spellings");
        assert_eq!(hit.paths[0].normalized, "src/auth/login.rs");
    }

    #[test]
    fn dot_and_parent_components_resolve_to_the_same_scope() {
        let plain = scoped("A", &["src/auth"]);
        for spelling in [
            "./src/auth",
            "src/./auth",
            "src/x/../auth",
            "src//auth/",
            "  src/auth  ",
        ] {
            let other = scoped("B", &[spelling]);
            assert!(
                plain.overlaps_scope(&other),
                "{spelling:?} should normalize to src/auth"
            );
        }
    }

    #[test]
    fn a_relative_path_cannot_escape_its_own_root() {
        // Path traversal in a scope declaration names ground outside the
        // project, so it is rejected rather than clamped to something usable.
        assert_eq!(parts("../outside"), None);
        assert_eq!(parts("src/../../outside"), None);
        assert_eq!(parts(".."), None);
    }

    #[test]
    fn an_absolute_path_may_step_above_root_because_root_is_its_floor() {
        assert_eq!(parts("/.."), Some(vec![]));
        assert_eq!(parts("/src/../src/auth"), Some(owned(&["src", "auth"])));
    }

    #[test]
    fn a_scope_containing_a_control_character_is_rejected() {
        assert_eq!(parts("src/a\u{0}b"), None);
        assert_eq!(parts("src/auth\nrm -rf"), None);
    }

    #[test]
    fn an_absolute_and_a_relative_declaration_of_the_same_tree_collide() {
        // Conservative on purpose: they may well be the same files, and a false
        // conflict costs a round while a miss costs a merge.
        let absolute = scoped("A", &["/src/auth"]);
        let relative = scoped("B", &["src/auth"]);
        assert!(absolute.overlaps_scope(&relative));
    }

    #[test]
    fn scope_comparison_does_not_vary_with_case() {
        // Documented limitation, pinned so it cannot change by accident: on a
        // case-insensitive filesystem `src/Auth` and `src/auth` are one
        // directory, and Orqyn does not see that. Matching is case-sensitive
        // everywhere so the same plan behaves identically on every platform.
        let lower = scoped("A", &["src/auth"]);
        let mixed = scoped("B", &["src/Auth"]);
        assert!(!lower.overlaps_scope(&mixed));
    }

    #[test]
    fn a_task_never_conflicts_with_itself() {
        let a = scoped("A", &["src/"]);
        assert!(!a.overlaps_scope(&a));
        assert!(a.scope_overlap(&a).is_none());
    }

    #[test]
    fn normalize_scope_paths_reports_nothing_usable_as_none() {
        assert!(normalize_scope_paths(&[]).is_none());
        assert!(normalize_scope_paths(&owned(&["", " ", "../x"])).is_none());
        assert_eq!(
            normalize_scope_paths(&owned(&["src/auth", ".."])),
            Some(vec![owned(&["src", "auth"])])
        );
    }

    #[test]
    fn a_declared_scope_survives_serde_and_the_task_stores_it() {
        let mut t = scoped("A", &[r"src\auth\"]);
        t.scope_paths = owned(&["src/auth/"]);
        let parsed: Task = serde_json::from_value(serde_json::to_value(&t).unwrap()).unwrap();
        assert_eq!(parsed.scope_paths, owned(&["src/auth/"]));
    }
}
