-- Blockers and task actions: the two things a recovery briefing could not
-- reconstruct before, because nothing recorded them.
--
-- Everything Orqyn already persisted - checkpoints, assignments, verifications,
-- status history, decisions, project state - was already reaching the briefing.
-- These two tables close the remaining gaps rather than duplicating what exists.

-- Blockers: what is stopping a task, and what clearing it would look like.
--
-- A blocker is retained after it stops blocking. `status` moves to `resolved` or
-- `obsolete` and `resolved_at` is stamped, so "we fixed it" stays distinguishable
-- from "we stopped trying" - the same append-only discipline the assignments and
-- decisions tables already use. Rows are never deleted.
CREATE TABLE IF NOT EXISTS blockers (
    id            TEXT PRIMARY KEY,
    task_id       TEXT NOT NULL REFERENCES tasks(id),
    kind          TEXT NOT NULL,
    status        TEXT NOT NULL,
    description   TEXT NOT NULL,
    resolution    TEXT,
    raised_by     TEXT,
    raised_at     TEXT NOT NULL,
    resolved_at   TEXT
);

CREATE INDEX IF NOT EXISTS idx_blockers_task ON blockers(task_id);
CREATE INDEX IF NOT EXISTS idx_blockers_status ON blockers(status);

-- Task actions: the observed activity a replacement agent needs to know what
-- already happened.
--
-- `task_id` is nullable because an action is context even when it belongs to no
-- task - exploration, a question. Those rows are kept for the same reason the
-- in-memory window was: "who else touched this?" includes work outside a task.
--
-- `at` is the second column of the index so that "the most recent N actions for
-- this task, in order" is a bounded index scan rather than a full sort.
--
-- Retention: this table is an append-only durable log and is not pruned. The
-- *briefing* is bounded - `RecentContext` keeps at most `max_actions` and
-- compacts the rest into a summary - so a replacement agent never receives an
-- unbounded window. Unbounded growth of the log itself is a known limitation,
-- not a claim that it cannot happen.
CREATE TABLE IF NOT EXISTS task_actions (
    id          TEXT PRIMARY KEY,
    task_id     TEXT REFERENCES tasks(id),
    agent_id    TEXT,
    session_id  TEXT,
    kind        TEXT NOT NULL,
    summary     TEXT NOT NULL,
    detail      TEXT,
    at          TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_task_actions_task ON task_actions(task_id, at);