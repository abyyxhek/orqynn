-- Orqyn — plans and decisions (migration 0002).
--
-- Phase 5's stated scope included plans and decisions; the tables did not
-- exist until now. Both are Orqyn-owned entities — neither substrate has a
-- plan or a decision concept — so they live here and nowhere else.
--
-- The invariants that matter:
--   - A project has at most one ACTIVE plan at a time. That is enforced by a
--     partial unique index, the same technique agent_assignments uses for
--     "at most one active assignment per task". Supersession is therefore not
--     a convention the caller honors; it is something the schema refuses to
--     let a second writer violate.
--   - Both tables are append-ish history: a superseded plan or decision is
--     UPDATEd in place to mark it, never deleted. "Why did we change approach"
--     and "what did we used to believe" stay answerable.
--
-- Status columns hold the serde form of their enum (snake_case JSON), as every
-- other status column does. A predicate that compares against a bare 'active'
-- never matches a column holding "active" — see the comment on
-- idx_assignments_active_per_task in 0001.

CREATE TABLE plans (
    id            TEXT PRIMARY KEY,
    project_id    TEXT NOT NULL REFERENCES projects(id),
    objective     TEXT NOT NULL,
    task_ids      TEXT NOT NULL,     -- JSON array of TaskId, in intended order
    rationale     TEXT NOT NULL,
    status        TEXT NOT NULL,     -- serde form of PlanStatus
    supersedes    TEXT,              -- the plan this one replaced, if any
    superseded_by TEXT,              -- set when this plan is superseded
    created_by    TEXT,              -- the agent that authorized it, if known
    state_version INTEGER NOT NULL DEFAULT 1,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);

-- One active plan per project, no more. A second activation must supersede the
-- sitting plan first; the index turns "forgot to supersede" into an error
-- rather than a silent second authoritative plan.
CREATE UNIQUE INDEX idx_plans_active_per_project
    ON plans(project_id) WHERE status = '"active"';

CREATE INDEX idx_plans_project     ON plans(project_id, created_at);
CREATE INDEX idx_plans_status      ON plans(status);

CREATE TABLE decisions (
    id                   TEXT PRIMARY KEY,
    task_id              TEXT,          -- provenance, not scope: a decision
                                         -- outlives its task
    title                TEXT NOT NULL,
    rationale            TEXT NOT NULL,
    alternatives         TEXT NOT NULL, -- JSON array of strings
    status               TEXT NOT NULL, -- serde form of DecisionStatus
    superseded_by        TEXT,
    made_by              TEXT,
    state_version        INTEGER NOT NULL DEFAULT 1,
    made_at              TEXT NOT NULL,
    updated_at           TEXT NOT NULL
);

CREATE INDEX idx_decisions_task    ON decisions(task_id, made_at);
CREATE INDEX idx_decisions_status  ON decisions(status);
