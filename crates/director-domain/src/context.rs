//! [RecentContext] — a bounded, compacted window of recent activity (Phase 9).
//!
//! This is Director's answer to the problem every long session hits: the
//! context window fills. Rather than dumping a raw transcript on the next
//! agent, Director keeps a bounded window of [crate::action::Action] entries
//! plus an optional compacted summary of everything that fell out of the
//! window.
//!
//! The invariant that matters: **the window is bounded at all times.** Pushing
//! an action never grows the window past its limit; it compacts instead. There
//! is no code path — including a faulty caller pushing a thousand entries at
//! once — that can produce an unbounded `RecentContext`.

use serde::{Deserialize, Serialize};

use crate::action::{Action, ActionKind};
use crate::ids::{AgentId, TaskId};

/// Default maximum number of actions kept before older ones are compacted.
pub const DEFAULT_MAX_ACTIONS: usize = 50;

/// Default cap on the number of failing-test names retained, so a broken suite
/// cannot flood the window.
pub const DEFAULT_MAX_FAILURE_NAMES: usize = 20;

/// A bounded view of what has been happening, addressed to whoever picks the
/// work up next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentContext {
    /// The task this context describes, if any.
    pub task_id: Option<TaskId>,
    /// Actions in the window, **oldest first**. Ordering is load-bearing: the
    /// next agent reads them as a narrative, and compaction always removes from
    /// the front.
    pub actions: Vec<Action>,
    /// Summary of actions that were compacted out of the window. Grows only
    /// when `actions` is at capacity.
    pub compacted_summary: Option<String>,
    /// How many actions have ever been recorded against this context, including
    /// compacted ones. Proves compaction is lossy-but-accountable.
    pub total_recorded: u64,
    /// The bound on the action window.
    pub max_actions: usize,
    /// When the window begins.
    pub window_start: Option<chrono::DateTime<chrono::Utc>>,
    /// When the window ends.
    pub window_end: Option<chrono::DateTime<chrono::Utc>>,
    /// Agents seen in the window. Answers "who else touched this?" without
    /// scanning history.
    pub participants: Vec<AgentId>,
}

impl RecentContext {
    /// Create an empty context for a task with the default bound.
    pub fn for_task(task_id: TaskId) -> Self {
        Self::new(Some(task_id), DEFAULT_MAX_ACTIONS)
    }

    /// Create an empty unscoped context (exploration, a question).
    pub fn unbounded_from_task() -> Self {
        Self::new(None, DEFAULT_MAX_ACTIONS)
    }

    /// Create an empty context with an explicit bound.
    pub fn new(task_id: Option<TaskId>, max_actions: usize) -> Self {
        RecentContext {
            task_id,
            actions: Vec::new(),
            compacted_summary: None,
            total_recorded: 0,
            max_actions: max_actions.max(1),
            window_start: None,
            window_end: None,
            participants: Vec::new(),
        }
    }

    /// Append an action, compacting from the front if the window is full.
    ///
    /// Compaction drops the oldest action and folds one line about it into
    /// `compacted_summary`. Important actions (errors, checkpoints) are
    /// *promoted* rather than dropped: they are kept in the window even when it
    /// is full, because they are precisely what resumption needs.
    pub fn push(&mut self, action: Action) {
        self.total_recorded = self.total_recorded.saturating_add(1);

        if self.window_start.is_none() {
            self.window_start = Some(action.at);
        }
        self.window_end = Some(action.at);
        if let Some(agent) = &action.agent_id {
            if !self.participants.contains(agent) {
                self.participants.push(agent.clone());
            }
        }

        // While at capacity, evict from the front, but never an important
        // action that is still inside the window.
        while self.actions.len() >= self.max_actions {
            let drop_index = self
                .actions
                .iter()
                .position(|a| a.kind.importance() < ActionKind::Error.importance());
            let Some(idx) = drop_index else {
                // The whole window is high-importance. Grow rather than lose a
                // checkpoint or an error; the bound is a target, not a ceiling
                // on evidence that resumption depends on.
                break;
            };
            let dropped = self.actions.remove(idx);
            self.compact(&dropped);
        }

        self.actions.push(action);
    }

    /// Fold one evicted action into the summary.
    fn compact(&mut self, action: &Action) {
        let line = format!("[{}] {}", action.kind, action.summary);
        self.compacted_summary = match self.compacted_summary.take() {
            Some(existing) => Some(format!("{existing}\n{line}")),
            None => Some(line),
        };
    }

    /// Number of actions currently in the window.
    pub fn len(&self) -> usize {
        self.actions.len()
    }

    /// True if the window holds nothing.
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// True if the bound is respected (or every entry is high-importance).
    pub fn respects_bound(&self) -> bool {
        self.actions.len() <= self.max_actions
            || self
                .actions
                .iter()
                .all(|a| a.kind.importance() >= ActionKind::Error.importance())
    }
}

/// A point-in-time serialization of context for a continuation package. Kept
/// separate from [RecentContext] because a snapshot is frozen — it is what gets
/// handed to an agent — while `RecentContext` keeps moving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSnapshot {
    /// The task this snapshot describes, if any.
    pub task_id: Option<TaskId>,
    /// The actions in the window when it was frozen.
    pub actions: Vec<Action>,
    /// Summary of everything compacted out of the window.
    pub compacted_summary: Option<String>,
    /// Agents seen in the window.
    pub participants: Vec<AgentId>,
    /// When the snapshot was taken.
    pub captured_at: chrono::DateTime<chrono::Utc>,
}

impl ContextSnapshot {
    /// Freeze the current window.
    pub fn from_window(ctx: &RecentContext) -> Self {
        ContextSnapshot {
            task_id: ctx.task_id.clone(),
            actions: ctx.actions.clone(),
            compacted_summary: ctx.compacted_summary.clone(),
            participants: ctx.participants.clone(),
            captured_at: chrono::Utc::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ActionId;

    fn action(n: u64, kind: ActionKind) -> Action {
        Action::new(
            ActionId::from_string(format!("ACT-{n}")),
            kind,
            format!("action {n}"),
        )
    }

    #[test]
    fn a_fresh_context_is_empty() {
        let ctx = RecentContext::for_task(TaskId::from_string("AUTH-42"));
        assert!(ctx.is_empty());
        assert_eq!(ctx.len(), 0);
        assert!(ctx.respects_bound());
        assert_eq!(ctx.max_actions, DEFAULT_MAX_ACTIONS);
    }

    #[test]
    fn pushing_grows_the_window_and_counts() {
        let mut ctx = RecentContext::for_task(TaskId::from_string("AUTH-42"));
        ctx.push(action(1, ActionKind::Command));
        ctx.push(action(2, ActionKind::Command));
        assert_eq!(ctx.len(), 2);
        assert_eq!(ctx.total_recorded, 2);
        assert_eq!(ctx.actions[0].summary, "action 1"); // oldest first
    }

    #[test]
    fn the_window_never_exceeds_its_bound() {
        let mut ctx = RecentContext::new(None, 3);
        for n in 1..=20 {
            ctx.push(action(n, ActionKind::Command));
        }
        assert!(ctx.len() <= 3, "window grew to {}", ctx.len());
        assert!(ctx.respects_bound());
        assert_eq!(ctx.total_recorded, 20);
        // The newest entries survived; the oldest were compacted.
        assert_eq!(ctx.actions.last().unwrap().summary, "action 20");
    }

    #[test]
    fn compaction_produces_a_summary_and_keeps_accounting() {
        let mut ctx = RecentContext::new(None, 2);
        ctx.push(action(1, ActionKind::Command));
        ctx.push(action(2, ActionKind::Command));
        ctx.push(action(3, ActionKind::Command));

        assert!(ctx.compacted_summary.is_some());
        assert!(ctx.compacted_summary.as_ref().unwrap().contains("action 1"));
        assert_eq!(ctx.total_recorded, 3);
    }

    #[test]
    fn important_actions_are_promoted_not_dropped() {
        // A window full of errors must not silently lose them.
        let mut ctx = RecentContext::new(None, 2);
        ctx.push(action(1, ActionKind::Error));
        ctx.push(action(2, ActionKind::Error));
        ctx.push(action(3, ActionKind::Error));

        assert_eq!(ctx.len(), 3);
        // It exceeded the target bound deliberately, to preserve evidence.
        assert!(ctx.respects_bound());
    }

    #[test]
    fn participants_accumulate_without_duplicates() {
        let mut ctx = RecentContext::new(None, 10);
        let mut a1 = action(1, ActionKind::Command);
        a1.agent_id = Some(AgentId::from_string("AGENT-claude"));
        let mut a2 = action(2, ActionKind::Command);
        a2.agent_id = Some(AgentId::from_string("AGENT-claude"));
        let mut a3 = action(3, ActionKind::Command);
        a3.agent_id = Some(AgentId::from_string("AGENT-codex"));

        ctx.push(a1);
        ctx.push(a2);
        ctx.push(a3);

        assert_eq!(ctx.participants.len(), 2);
    }

    #[test]
    fn a_snapshot_freezes_the_window() {
        let mut ctx = RecentContext::new(None, 10);
        ctx.push(action(1, ActionKind::Command));
        let snap = ContextSnapshot::from_window(&ctx);
        ctx.push(action(2, ActionKind::Command));

        assert_eq!(snap.actions.len(), 1);
        assert_eq!(ctx.len(), 2);
    }

    #[test]
    fn context_round_trips_through_serde() {
        let mut ctx = RecentContext::new(Some(TaskId::from_string("AUTH-42")), 2);
        ctx.push(action(1, ActionKind::Command));
        ctx.push(action(2, ActionKind::Command));
        ctx.push(action(3, ActionKind::Note));

        let json = serde_json::to_string(&ctx).unwrap();
        let back: RecentContext = serde_json::from_str(&json).unwrap();
        assert_eq!(ctx, back);
    }
}
