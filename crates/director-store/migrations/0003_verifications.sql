-- Orqyn — verifications (migration 0003).
--
-- Phase 10. The verification engine judges a task's work by evidence it gathers
-- itself, and a verification is the durable record of one such judgment: what
-- was asked, what was found, and what Orqyn concluded. Without this table the
-- verdict is ephemeral — a task moves to `done`, and nothing afterwards can say
-- *why*, which is exactly the question an audit or a confused human asks first.
--
-- The invariants that matter:
--   - A verification is append-only history. Re-judging a task writes a new row
--     with its own id; nothing here is ever UPDATEd for that purpose. A task
--     that failed twice and passed on the third attempt carries three rows, and
--     the story of the work is the order of them.
--   - The status column holds the serde form of `VerificationStatus`, as every
--     other status column does. A predicate comparing against a bare 'passed'
--     never matches a column holding "passed" — the same trap
--     `idx_plans_active_per_project` guards against in 0002.
--   - Evidence is a JSON array, because it has no query life of its own: it is
--     read whole with the verification it describes, the way a task's expected
--     outputs are.

CREATE TABLE verifications (
    id             TEXT PRIMARY KEY,
    task_id        TEXT NOT NULL REFERENCES tasks(id),
    project_id     TEXT NOT NULL REFERENCES projects(id),
    repository_id  TEXT,                     -- NULL when no repository is registered
    status         TEXT NOT NULL,            -- serde form of VerificationStatus
    evidence       TEXT NOT NULL,            -- JSON array of Evidence
    head_commit    TEXT,                     -- NULL when no repository was available
    state_version  INTEGER NOT NULL DEFAULT 1,
    created_at     TEXT NOT NULL
);

-- The history of Orqyn's judgment of one task, newest first.
CREATE INDEX idx_verifications_task ON verifications(task_id, created_at);

-- "What did Orqyn conclude, and when" across a project — the audit view.
CREATE INDEX idx_verifications_project ON verifications(project_id, created_at);
CREATE INDEX idx_verifications_status ON verifications(status);
