//! The dependency graph over a plan's tasks — validation and deterministic
//! ordering.
//!
//! This is pure domain logic with no storage and no substrate: it answers two
//! questions about a set of tasks and the edges between them, and it answers
//! them the same way every time.
//!
//! 1. **Is the graph a plan at all?** A task cannot depend on itself, cannot
//!    depend on a task that is not in the plan, and the edges cannot form a
//!    cycle. Any of those makes the plan unexecutable, and the point of
//!    checking here rather than at assignment time is that a bad plan is
//!    rejected before anything is written, not after a task is blocked by its
//!    own descendant.
//!
//! 2. **In what order should the tasks run?** [`order`] returns a topological
//!    order that is deterministic in the *input*, not in the clock or in
//!    hashmap iteration order. Ties are broken by the order the caller listed
//!    the tasks, so the same plan spec always yields the same sequence.
//!
//! The planner (Phase 6's PLAN step) builds a [`PlanGraph`] from the
//! decomposition it is handed, validates it, and only then persists anything.
//! The REPLAN step will reuse the same validation when it revises a graph.

use std::collections::{HashMap, HashSet};

use crate::ids::TaskId;

/// One task in the graph, plus the tasks it depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanNode {
    /// This task's identifier.
    pub id: TaskId,
    /// Tasks that must reach a terminal state before this one starts.
    pub dependencies: Vec<TaskId>,
}

impl PlanNode {
    /// A task with no dependencies.
    pub fn leaf(id: TaskId) -> Self {
        PlanNode {
            id,
            dependencies: vec![],
        }
    }

    /// A task that depends on `deps`.
    pub fn with_dependencies(id: TaskId, deps: Vec<TaskId>) -> Self {
        PlanNode {
            id,
            dependencies: deps,
        }
    }
}

/// What is wrong with a proposed dependency graph.
///
/// Deliberately concrete: a caller reports these to a human, and "task AUTH-2
/// depends on AUTH-9, which is not in the plan" is actionable in a way that
/// "invalid graph" is not.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraphError {
    /// A task lists itself as a dependency. This is the one error that is a
    /// caller bug rather than a modeling mistake.
    #[error("task {0} cannot depend on itself")]
    SelfDependency(TaskId),

    /// A task depends on a task that is not part of the plan. Dependencies on
    /// tasks outside the plan are refused rather than assumed: a task Orqyn
    /// does not own is a premise it cannot check.
    #[error("task {task} depends on {dependency}, which is not in the plan")]
    UnknownDependency {
        /// The task holding the dangling edge.
        task: TaskId,
        /// The task it pointed at, which is not in the plan.
        dependency: TaskId,
    },

    /// The edges form a cycle, so no task in it can ever become ready. Carries
    /// one cycle found, in edge order, so the message names the actual loop.
    #[error("circular dependency: {}", cycle.join(" → "))]
    Cycle {
        /// The ids around the cycle, in order, repeating the first id at the
        /// end so the loop is visible in the message.
        cycle: Vec<String>,
    },
}

/// Check a proposed graph and return the tasks in a deterministic execution
/// order.
///
/// This is the single entry point the planner uses: validation and ordering are
/// one operation because ordering is what a cycle *means* — a graph that cannot
/// be ordered has a cycle, so separating the two would only duplicate the walk.
///
/// The order is topological, with ties broken by the order tasks appear in
/// `nodes`. That is what makes it deterministic: the same input always yields
/// the same output, regardless of hashmap iteration order or the wall clock.
/// Dependencies are listed as "run after", so a task with no dependencies comes
/// first.
pub fn order(nodes: &[PlanNode]) -> Result<Vec<TaskId>, GraphError> {
    validate_shape(nodes)?;

    // Index each task by its position in the caller's list. Everything below
    // keys off this map, so `nodes` is read only through it and the input
    // order — not pointer order or hash order — drives every decision.
    let position: HashMap<&TaskId, usize> =
        nodes.iter().enumerate().map(|(i, n)| (&n.id, i)).collect();

    // Kahn's algorithm, but with a stable tie-break: at each step, among the
    // tasks whose dependencies are all already placed, the one the caller
    // listed earliest goes next. A plain Kahn implementation using a hashmap
    // or a heap of TaskIds would be nondeterministic in the tie-break, which
    // would make plan ordering vary between runs of the same spec.
    let mut remaining_deps: HashMap<&TaskId, HashSet<&TaskId>> = nodes
        .iter()
        .map(|n| (&n.id, n.dependencies.iter().collect::<HashSet<_>>()))
        .collect();

    let mut placed: Vec<TaskId> = Vec::with_capacity(nodes.len());
    let mut done: HashSet<&TaskId> = HashSet::new();

    while placed.len() < nodes.len() {
        // Candidates: not yet placed, and every dependency is already placed.
        // Filter to the *earliest-listed* one for determinism.
        let next = nodes
            .iter()
            .filter(|n| !done.contains(&n.id))
            .filter(|n| remaining_deps[&n.id].is_subset(&done))
            .min_by_key(|n| position[&n.id]);

        match next {
            Some(node) => {
                placed.push(node.id.clone());
                done.insert(&node.id);
                remaining_deps.remove(&node.id);
            }
            None => {
                // Nothing is placeable and work remains, so the unfinished
                // tasks form one or more cycles. Report one concretely.
                let stuck: Vec<&TaskId> = remaining_deps.keys().copied().collect();
                return Err(find_cycle(nodes, &stuck));
            }
        }
    }

    Ok(placed)
}

/// Shape checks that do not need the ordering walk: self-dependency, unknown
/// dependency. Kept separate from [`order`] only because they are cheap
/// preconditions a caller might want to check without ordering.
pub fn validate_shape(nodes: &[PlanNode]) -> Result<(), GraphError> {
    let known: HashSet<&TaskId> = nodes.iter().map(|n| &n.id).collect();

    for node in nodes {
        for dep in &node.dependencies {
            if dep == &node.id {
                return Err(GraphError::SelfDependency(node.id.clone()));
            }
            if !known.contains(dep) {
                return Err(GraphError::UnknownDependency {
                    task: node.id.clone(),
                    dependency: dep.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Find one cycle among `stuck` and return it as a [`GraphError::Cycle`].
///
/// `stuck` is the set of tasks the ordering walk could not place, which is
/// guaranteed to contain a cycle (that is why the walk stopped). This walks the
/// edges among them until it revisits a task, which is the loop.
fn find_cycle(nodes: &[PlanNode], stuck: &[&TaskId]) -> GraphError {
    let stuck_set: HashSet<&TaskId> = stuck.iter().copied().collect();
    let by_id: HashMap<&TaskId, &PlanNode> = nodes.iter().map(|n| (&n.id, n)).collect();

    // Start from the earliest-listed stuck task, so the cycle reported is
    // stable for a given input rather than depending on iteration order.
    let start = stuck
        .iter()
        .min_by_key(|t| {
            nodes
                .iter()
                .position(|n| &n.id == **t)
                .unwrap_or(usize::MAX)
        })
        .copied()
        .unwrap();

    let mut path: Vec<&TaskId> = vec![start];
    let mut current = start;

    loop {
        // Follow an edge to a task that is also stuck. Prefer the first
        // dependency in the caller's declared order, again for determinism.
        let next = by_id[current]
            .dependencies
            .iter()
            .find(|dep| stuck_set.contains(dep));

        match next {
            None => break,
            Some(step) => {
                if step == start || path.contains(&step) {
                    path.push(step);
                    break;
                }
                path.push(step);
                current = step;
            }
        }
    }

    GraphError::Cycle {
        cycle: path.iter().map(|t| t.to_string()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str) -> TaskId {
        TaskId::from_string(id)
    }

    fn node(id: &str, deps: &[&str]) -> PlanNode {
        PlanNode::with_dependencies(task(id), deps.iter().map(|d| task(d)).collect())
    }

    #[test]
    fn an_empty_graph_orders_to_nothing() {
        assert!(order(&[]).unwrap().is_empty());
    }

    #[test]
    fn independent_tasks_keep_the_callers_order() {
        // No edges, so the order is the input order, not a hash order.
        let nodes = vec![node("C", &[]), node("A", &[]), node("B", &[])];

        assert_eq!(
            order(&nodes).unwrap(),
            vec![task("C"), task("A"), task("B")],
            "ties break on the caller's listing order"
        );
    }

    #[test]
    fn a_linear_graph_orders_root_first() {
        // A → B → C, declared out of order: the order must still be A, B, C.
        let nodes = vec![node("C", &["B"]), node("B", &["A"]), node("A", &[])];

        assert_eq!(
            order(&nodes).unwrap(),
            vec![task("A"), task("B"), task("C")]
        );
    }

    #[test]
    fn a_diamond_orders_before_the_task_that_waits() {
        // A → B, A → C, B → D, C → D: D is last, B precedes C because it was
        // listed first.
        let nodes = vec![
            node("D", &["B", "C"]),
            node("B", &["A"]),
            node("C", &["A"]),
            node("A", &[]),
        ];

        assert_eq!(
            order(&nodes).unwrap(),
            vec![task("A"), task("B"), task("C"), task("D")],
            "the diamond's shared root comes first and the join comes last"
        );
    }

    #[test]
    fn ordering_is_stable_across_repeated_calls() {
        // The same input must always produce the same output. Repeated with
        // fresh maps each call; a hash-order leak would show up as a flake.
        let nodes = vec![
            node("D", &["B", "C"]),
            node("C", &["A"]),
            node("B", &["A"]),
            node("E", &[]),
            node("A", &[]),
        ];

        let first = order(&nodes).unwrap();
        for _ in 0..20 {
            assert_eq!(order(&nodes).unwrap(), first, "order drifted");
        }
    }

    #[test]
    fn a_self_dependency_is_rejected() {
        let nodes = vec![node("A", &["A"])];

        let err = order(&nodes).unwrap_err();
        assert!(matches!(err, GraphError::SelfDependency(t) if t == task("A")));
    }

    #[test]
    fn a_dependency_outside_the_plan_is_rejected() {
        let nodes = vec![node("A", &["B"]), node("B", &["NOPE"])];

        let err = order(&nodes).unwrap_err();
        assert!(
            matches!(err, GraphError::UnknownDependency { task: ref t, dependency: ref d }
                     if t == &task("B") && d == &task("NOPE")),
            "got {err:?}"
        );
    }

    #[test]
    fn a_simple_cycle_is_detected_and_named() {
        // A → B → C → A.
        let nodes = vec![node("A", &["C"]), node("B", &["A"]), node("C", &["B"])];

        let err = order(&nodes).unwrap_err();
        match err {
            GraphError::Cycle { cycle } => {
                // The cycle closes on itself, and names the tasks involved.
                assert_eq!(
                    cycle.first(),
                    cycle.last(),
                    "the cycle should close on itself: {cycle:?}"
                );
                assert!(
                    cycle.contains(&"A".to_string())
                        && cycle.contains(&"B".to_string())
                        && cycle.contains(&"C".to_string()),
                    "every task in the cycle appears: {cycle:?}"
                );
            }
            other => panic!("expected a cycle, got {other:?}"),
        }
    }

    #[test]
    fn a_two_node_cycle_is_detected() {
        let nodes = vec![node("A", &["B"]), node("B", &["A"])];

        assert!(matches!(
            order(&nodes).unwrap_err(),
            GraphError::Cycle { .. }
        ));
    }

    #[test]
    fn a_cycle_is_found_even_among_otherwise_ordered_tasks() {
        // D depends on a cycle between A and B, so nothing can be placed.
        let nodes = vec![node("D", &["A"]), node("A", &["B"]), node("B", &["A"])];

        assert!(matches!(
            order(&nodes).unwrap_err(),
            GraphError::Cycle { .. }
        ));
    }

    #[test]
    fn duplicate_dependencies_are_tolerated_and_do_not_create_a_cycle() {
        // The same edge declared twice is redundant, not a cycle.
        let nodes = vec![node("A", &[]), node("B", &["A", "A"])];

        assert_eq!(order(&nodes).unwrap(), vec![task("A"), task("B")]);
    }

    #[test]
    fn validate_shape_catches_the_same_errors_without_ordering() {
        assert!(validate_shape(&[node("A", &["A"])]).is_err());
        assert!(validate_shape(&[node("A", &["NOPE"])]).is_err());
        assert!(validate_shape(&[node("A", &[]), node("B", &["A"])]).is_ok());
    }
}
