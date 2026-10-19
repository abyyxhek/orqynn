-- Handoffs: an auditable record that one agent transferred continuation
-- responsibility for a task to another.
--
-- This table is deliberately NOT the assignment table and NOT a second source
-- of task state:
--
--   * Execution authority lives in `assignments`, under the lease and scope
--     guards. Accepting a handoff grants nothing; a receiving agent still needs
--     an assignment before it may act.
--   * Task state, verification evidence and resume status live in `tasks`,
--     `verifications` and `task_actions`. A handoff records what an outgoing
--     agent *said*, which is context and never proof of completion.
--   * The continuation narrative is an immutable snapshot taken at creation, so
--     the record stays auditable after the task moves on.
--
-- `source_checkpoint` and `source_task_version` are what make a stale snapshot
-- detectable: they record the exact state the narrative was derived from, so a
-- reader can ask "is this still current?" instead of guessing. Both are
-- nullable and are meant to be - a task observed before it was ever in progress
-- has no checkpoint, and inventing one would be a fabrication. A NULL means
-- "this snapshot has no recorded derivation point", which reads as stale rather
-- than as current.
--
-- `project_id` is nullable for the same reason: a handoff can only name a
-- project Orqyn actually observed, and a record that cannot name one is still
-- worth keeping for attribution.
CREATE TABLE IF NOT EXISTS handoffs (
    id                  TEXT PRIMARY KEY,
    task_id             TEXT NOT NULL REFERENCES tasks(id),
    project_id          TEXT,
    from_agent          TEXT NOT NULL,
    from_session        TEXT,
    to_agent            TEXT NOT NULL,
    state               TEXT NOT NULL,
    summary             TEXT NOT NULL,
    open_questions      TEXT NOT NULL,
    next_steps          TEXT NOT NULL,
    files_touched       TEXT NOT NULL,
    source_checkpoint   TEXT,
    source_task_version INTEGER,
    created_at          TEXT NOT NULL,
    accepted_at         TEXT,
    accepted_by_session TEXT
);

-- The open-handoffs query is "everything still waiting to be claimed", which is
-- the only enumeration this table ever serves. `state` leads the index so that
-- query is a bounded scan rather than a full sort.
CREATE INDEX IF NOT EXISTS idx_handoffs_state ON handoffs(state);
-- Attribution reads are always per task, and always newest first.
CREATE INDEX IF NOT EXISTS idx_handoffs_task ON handoffs(task_id, created_at);

-- Handoffs are retained after they stop being open, for the same append-only
-- reason blockers and decisions are: "who passed what to whom, and when" has to
-- stay answerable after the fact. Rows are never deleted.