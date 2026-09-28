-- Director Brain — initial schema (migration 0001).
--
-- This is the *only* place Director's tables are created. The application never
-- issues CREATE TABLE at runtime; the schema is owned by the migration runner.
--
-- One table is deliberately *not* created here: schema_migrations, the runner's
-- own bookkeeping. The runner has to create it before it can read what has been
-- applied — otherwise it could not tell whether this migration has already run,
-- which is exactly the question that makes re-running `open` safe. It is
-- bootstrapped in Rust (see `ensure_schema_table`), and every *Director* table
-- is created here.
--
-- Conventions used throughout:
--   - Every id is TEXT. Director ids are human-readable strings ("AUTH-42"),
--     which is the whole point of the id model: they survive across machines,
--     harnesses, and substrates. They are the primary key directly.
--   - Every mutable table carries state_version INTEGER NOT NULL DEFAULT 1,
--     the optimistic-concurrency column. Updates are
--       UPDATE ... SET state_version = state_version + 1
--         WHERE id = ? AND state_version = ?
--     and a zero row count is the conflict.
--   - Timestamps are TEXT RFC3339 (UTC), matching what chrono emits.
--   - Collections that have no query life of their own (a task's dependencies,
--     scope paths, expected outputs) are JSON columns rather than join tables.
--     They are loaded and written as one unit with their owner, and nothing
--     ever selects "all tasks depending on X" through them — that query goes
--     through the task_dependencies table, which exists precisely because that
--     query does have a life.
--   - SQLite's dynamic typing means a column declared INTEGER accepts a text
--     value. The store's Rust serialization is what keeps types honest; the
--     declarations document intent and are what a future strict-mode migration
--     would enforce.

-- ---------------------------------------------------------------------------
-- Projects: the root everything else hangs off.
-- ---------------------------------------------------------------------------
CREATE TABLE projects (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    root            TEXT NOT NULL,
    default_branch  TEXT NOT NULL,           -- serde form of DefaultBranch
    repository_id   TEXT,                    -- reference, not an inlined record
    state_version   INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

-- Registered repositories, referenced from projects and from stored project
-- state. Only the fields Director needs as a *link*; the git observer remains
-- the owner of the repository's own detail (see the phase doc).
CREATE TABLE repositories (
    id              TEXT PRIMARY KEY,
    project_id      TEXT NOT NULL REFERENCES projects(id),
    local_path      TEXT NOT NULL,
    remote_url      TEXT,
    created_at      TEXT NOT NULL
);

CREATE INDEX idx_repositories_project ON repositories(project_id);

-- ---------------------------------------------------------------------------
-- Tasks.
--
-- Deliberately no current_agent_id column. Assignment is the
-- agent_assignments table; a task row never names an agent.
-- ---------------------------------------------------------------------------
CREATE TABLE tasks (
    id                    TEXT PRIMARY KEY,
    project_id            TEXT NOT NULL REFERENCES projects(id),
    title                 TEXT NOT NULL,
    objective             TEXT NOT NULL,
    description           TEXT,
    status                TEXT NOT NULL,
    priority              TEXT,              -- serde form of Priority, nullable
    complexity            TEXT NOT NULL,
    expected_outputs      TEXT NOT NULL,     -- JSON array of ExpectedOutput
    dependencies          TEXT NOT NULL,     -- JSON array of TaskId (denormalized;
                                             -- see task_dependencies for the queryable form)
    scope_paths           TEXT NOT NULL,     -- JSON array of strings
    required_capabilities TEXT NOT NULL,     -- JSON array of Capability
    subtasks              TEXT NOT NULL,     -- JSON array of Subtask
    state_version         INTEGER NOT NULL DEFAULT 1,
    created_at            TEXT NOT NULL,
    updated_at            TEXT NOT NULL
);

-- The queryable form of the dependency graph. The JSON column above is how a
-- task carries its dependencies; this table is how the store answers "what is
-- blocked by X" and "what is ready now" without scanning every task.
CREATE TABLE task_dependencies (
    task_id       TEXT NOT NULL REFERENCES tasks(id),
    dependency_id TEXT NOT NULL,
    PRIMARY KEY (task_id, dependency_id)
);

CREATE INDEX idx_tasks_project_status ON tasks(project_id, status);
CREATE INDEX idx_tasks_status         ON tasks(status);
CREATE INDEX idx_tasks_updated        ON tasks(updated_at);

-- Append-only task status history. Written on every status change; never
-- updated, never deleted. The id is a surrogate because a transition is an
-- event, not an entity with a stable name — and a task can move between the
-- same two states more than once, so (task, from, to) is not a key.
CREATE TABLE task_status_history (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id       TEXT NOT NULL,
    from_status   TEXT NOT NULL,
    to_status     TEXT NOT NULL,
    occurred_at   TEXT NOT NULL
);

CREATE INDEX idx_task_history_task ON task_status_history(task_id, id);

-- ---------------------------------------------------------------------------
-- Agents: the registry of who is available.
-- ---------------------------------------------------------------------------
CREATE TABLE agents (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    harness         TEXT NOT NULL,          -- serde form of Harness
    model           TEXT,
    machine         TEXT NOT NULL,
    capabilities    TEXT NOT NULL,          -- JSON array of Capability
    status          TEXT NOT NULL,
    current_task    TEXT,                   -- denormalized view; the assignment
                                             -- record is the authority
    provider        TEXT,
    metadata        TEXT,                   -- JSON, free-form attribution only
    state_version   INTEGER NOT NULL DEFAULT 1,
    registered_at   TEXT NOT NULL,
    last_seen       TEXT NOT NULL,
    updated_at      TEXT
);

CREATE INDEX idx_agents_status  ON agents(status);
CREATE INDEX idx_agents_machine ON agents(machine);

-- ---------------------------------------------------------------------------
-- Agent sessions: one invocation of one agent on one machine.
--
-- A task accumulates many sessions; none of them are the task.
-- ---------------------------------------------------------------------------
CREATE TABLE agent_sessions (
    id                TEXT PRIMARY KEY,
    project_id        TEXT NOT NULL REFERENCES projects(id),
    agent_id          TEXT NOT NULL REFERENCES agents(id),
    machine_id        TEXT NOT NULL,
    task_id           TEXT,
    status            TEXT NOT NULL,
    parent_session_id TEXT,
    branch            TEXT,
    commit_sha        TEXT,
    started_at        TEXT NOT NULL,
    ended_at          TEXT,
    end               TEXT,                 -- serde form of SessionEnd
    workdir           TEXT,
    last_seen         TEXT,
    state_version     INTEGER NOT NULL DEFAULT 1
);

CREATE INDEX idx_sessions_task   ON agent_sessions(task_id, started_at);
CREATE INDEX idx_sessions_agent  ON agent_sessions(agent_id, started_at);
CREATE INDEX idx_sessions_project ON agent_sessions(project_id, started_at);

-- ---------------------------------------------------------------------------
-- Agent assignments: the first-class link between a task and an agent for a
-- period.
--
-- The task_outlives_agent invariant lives here. Releasing marks a row; it never
-- deletes it. That is what makes assignment history queryable.
-- ---------------------------------------------------------------------------
CREATE TABLE agent_assignments (
    id              TEXT PRIMARY KEY,
    task_id         TEXT NOT NULL REFERENCES tasks(id),
    agent_id        TEXT NOT NULL REFERENCES agents(id),
    session_id      TEXT REFERENCES agent_sessions(id),
    status          TEXT NOT NULL,          -- proposed | active | released
    assigned_at     TEXT NOT NULL,
    released_at     TEXT,
    release_reason  TEXT,
    note            TEXT,
    state_version   INTEGER NOT NULL DEFAULT 1
);

-- The "at most one active assignment per task" rule is enforced by a partial
-- unique index: only one row per task may have status 'active'. This is the
-- unambiguity invariant, expressed where a concurrent writer cannot talk its
-- way past it.
--
-- The predicate must compare against the *stored* form of the column. Status
-- enums are written as JSON text by the repositories, which for a snake_case
-- unit variant is the quoted string "active" (eight characters, including the
-- double quotes) — not the bare six-character `active`. A predicate written
-- against the bare literal silently matches nothing, and the index becomes
-- inert: the rule is then unenforced and two assignments can go active on one
-- task. This was caught by the store test suite, not by reading the schema.
CREATE UNIQUE INDEX idx_assignments_active_per_task
    ON agent_assignments(task_id) WHERE status = '"active"';

CREATE INDEX idx_assignments_task  ON agent_assignments(task_id, assigned_at);
CREATE INDEX idx_assignments_agent ON agent_assignments(agent_id, assigned_at);
CREATE INDEX idx_assignments_status ON agent_assignments(status);

-- ---------------------------------------------------------------------------
-- Checkpoints: Director's own resumption documents.
--
-- Superseded checkpoints are retained, not deleted.
-- ---------------------------------------------------------------------------
CREATE TABLE checkpoints (
    id                   TEXT PRIMARY KEY,
    task_id              TEXT NOT NULL REFERENCES tasks(id),
    objective            TEXT NOT NULL,
    progress             TEXT NOT NULL,
    progress_fraction    REAL NOT NULL,
    branch               TEXT,
    commit_sha           TEXT,
    changed_files        TEXT NOT NULL,     -- JSON array of strings
    test_results         TEXT,              -- JSON TestResults, nullable
    current_blocker      TEXT,
    important_decisions  TEXT NOT NULL,     -- JSON array of DecisionId
    current_assumptions  TEXT NOT NULL,     -- JSON array of strings
    next_action          TEXT NOT NULL,
    context_version      INTEGER NOT NULL,
    project_state_version INTEGER,
    task_state_version   INTEGER,
    status               TEXT NOT NULL,          -- JSON serde form of CheckpointStatus;
                                             -- no DEFAULT: every insert writes it,
                                             -- and a bare-'current' default would
                                             -- not match the JSON form anyway
    created_at           TEXT NOT NULL,
    created_by_agent     TEXT,
    created_by_session   TEXT,
    state_version        INTEGER NOT NULL DEFAULT 1
);

-- The "one current checkpoint per task" rule, enforced the same way as the
-- assignment rule: a partial unique index, not a convention. See the note on
-- idx_assignments_active_per_task — the predicate matches the JSON-encoded
-- column value, so it carries the double quotes.
CREATE UNIQUE INDEX idx_checkpoints_current_per_task
    ON checkpoints(task_id) WHERE status = '"current"';

CREATE INDEX idx_checkpoints_task ON checkpoints(task_id, created_at);

-- ---------------------------------------------------------------------------
-- Normalized project state: what Director currently knows.
--
-- One row per project, replaced on each observation. This is *not* the git
-- observation itself; the git observer owns that, and this is the normalized
-- belief Director holds between observations.
-- ---------------------------------------------------------------------------
CREATE TABLE project_states (
    project_id         TEXT PRIMARY KEY REFERENCES projects(id),
    repository_id      TEXT,
    branch             TEXT,
    head_commit        TEXT NOT NULL,
    working_tree_clean INTEGER NOT NULL,    -- 0/1; SQLite has no BOOLEAN
    observation_version INTEGER NOT NULL,
    last_observed_at   TEXT NOT NULL,
    state_version      INTEGER NOT NULL DEFAULT 1
);

-- ---------------------------------------------------------------------------
-- Provider synchronization metadata.
--
-- A pointer and a status, not a replica of the provider's schema.
-- ---------------------------------------------------------------------------
CREATE TABLE provider_sync (
    entity_id        TEXT NOT NULL,
    provider         TEXT NOT NULL,
    external_id      TEXT,
    last_sync_at     TEXT NOT NULL,
    last_success_at  TEXT,
    last_error       TEXT,
    external_version TEXT,
    PRIMARY KEY (entity_id, provider)
);

CREATE INDEX idx_provider_sync_provider ON provider_sync(provider, last_sync_at);
