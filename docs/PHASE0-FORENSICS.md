# Phase 0 — Repository Forensics: ai-memory vs handoff-mcp

> Director Brain · Phase 0 deliverable
> Date: 2026-09-24
> Scope: READ-ONLY audit of both upstream repositories. No code was modified.

Repositories audited (both already cloned into the workspace):

| Repo | Path | Version | License | Scale |
|---|---|---|---|---|
| `akitaonrails/ai-memory` | `ai_memory/ai-memory` | 2.4.0 | MIT (© 2026 Fabio Akita) | 11 crates, ~307 Rust files, ~261k LOC, edition 2024, rustc 1.95 |
| `alphaelements/handoff-mcp` | `ai_memory/handoff-mcp` | 0.35.1 | MIT (© 2026 Fabio Akita) | 1 crate + 5 plugin dirs, ~104 Rust files, ~26k LOC, edition 2021, rustc 1.85 |

Both are MIT. Both are Rust. That is where the similarity ends.

---

## 1. ai-memory — Architecture Summary

### 1.1 What it is

ai-memory is a **long-term memory system for coding agents**, not an orchestrator. Its own
framing (`docs/ARCHITECTURE.md:9-21`): a single Rust binary that gives MCP-capable coding
agents long-term memory shared across CLIs — "quit one mid-task; open another in the same
directory; continue."

The artifact it accretes is a **Karpathy-style LLM wiki**: a git-versioned tree of markdown
pages on disk, compiled over time, appended-to. Pages version in place via supersession,
semantic concepts compound, episodic logs decay. A companion SQLite index gives FTS5 +
lexical entity + link-neighbor retrieval, with optional vectors. **The markdown stays the
source of truth; SQLite is the derived index.**

### 1.2 Workspace layout (11 crates)

| Crate | Role |
|---|---|
| `ai-memory-core` | Domain model: pages, blocks, sessions, observations, handoffs, messages, routing skills |
| `ai-memory-store` | SQLite persistence: single-writer actor, migrations, FTS5, sqlite-vec, decay, audit |
| `ai-memory-wiki` | Markdown wiki read/write, supersession, split/reassemble, migrations |
| `ai-memory-mcp` | MCP server (the tool surface the agents see) |
| `ai-memory-hooks` | Lifecycle hook capture → observations |
| `ai-memory-llm` | LLM provider abstraction, embeddings, local embeddings |
| `ai-memory-consolidate` | Session-summary rewriting, auto-improvement scheduler/proposals |
| `ai-memory-web` | HTTP server (web UI + REST API + the `/hook` ingestion endpoint) |
| `ai-memory-cli` | CLI: `hook`, `run`, `serve`, `backup`, `restore`, `install-hooks`, `install-mcp`, … |
| `ai-memory-workstream` | Managed cross-harness workstream launcher (`ai-memory run`) |
| `ai-memory-test-support` | Shared test fixtures (`publish = false`) |

### 1.3 Data model (canonical entities)

From `crates/ai-memory-store/src/lib.rs` exports and the migration set:

- **Page** — a markdown file in the wiki tree. Canonical frontmatter (`entities`, `kind`,
  `tier`, `pinned`, `expires_at`, `author_id`). Kinds: concepts, decisions, gotchas,
  procedures, `_rules`, sessions. Versioned by supersession.
- **Observation** — a sanitized lifecycle-hook event (prompt, command, tool call, file
  change, error). Raw JSONL segments are immutable. FTS-indexed (`V07`).
- **Session** — `sessions` table, with `agent_kind` (Claude, Codex, Cursor, Gemini, OpenCode,
  Devin, Grok, Kimi, Pi, Antigravity, Zero… — one migration per agent kind, `V09`–`V30`),
  `ended_observation_count`.
- **Handoff** — typed claim-once snapshot. Schema `V02__handoffs.sql`:

  ```sql
  CREATE TABLE handoffs (
      id, workspace_id, project_id, from_session_id, from_agent, to_agent, cwd,
      summary, open_questions, next_steps, files_touched,
      state TEXT CHECK (state IN ('open','accepted','expired')),
      created_at, accepted_by, accepted_at, accepted_by_session
  );
  ```

- **Message** — cross-project agent messaging (`memory_message_send/pop/list/cancel`).
- **Workstream** — managed cross-harness continuity ledger (native source/delivery cursors,
  idempotent retry state).

### 1.4 Storage model

- Embedded **SQLite** via `rusqlite`, at `<data_dir>/db/memory.sqlite`, **WAL mode**, foreign
  keys on. All migrations run at startup (`V01`–`V30+`, SQL files in
  `crates/ai-memory-store/migrations/`).
- **Single-writer actor**: every mutation serializes through a dedicated OS thread
  (`writer.rs`) behind a `WriterHandle`. Readers use a `ReaderPool`.
- Wiki markdown lives in a separate git-versioned directory — the source of truth.
- **Not a server by default**: local binary mode, or Docker image (`akitaonrails/ai-memory`,
  linux/amd64 + arm64) with loopback binding and optional bearer token. Multiuser and
  per-project authz are designed (`docs/design-per-project-authz.md`) with API keys
  (`api_credentials.rs`, `password.rs`, `users.rs`, `web_sessions.rs`).
- **Backups**: `ai-memory backup --to <tarball>` uses SQLite's online backup API (source stays
  writable); `restore` reverses it. Or `git push` the wiki dir + `rsync` the data dir.

### 1.5 MCP tool surface (~30 tools, all `memory_*`-prefixed)

Retrieval: `memory_query`, `memory_recent`, `memory_read_page`, `memory_explore`,
`memory_briefing`, `memory_read_session_observations`, `memory_feedback`.
Wiki write: `memory_write_page`, `memory_delete_page`, `memory_lint`, `memory_forget_sweep`.
Handoff: `memory_handoff_begin`, `memory_handoff_accept`, `memory_handoff_cancel`,
`memory_handoff_list`. Messaging: `memory_message_send`, `memory_message_pop`,
`memory_message_list`, `memory_message_cancel`. Maintenance: `memory_consolidate`,
`memory_auto_improve`, `memory_status`, `memory_install_self_routing`.

Transport: stdio MCP for local; HTTP/SSE-capable server via `ai-memory-web`.

### 1.6 Lifecycle hooks and capture

Agents emit lifecycle hooks (SessionStart, UserPromptSubmit, PostToolUse, SessionEnd).
Shell-script hooks `curl` event JSON to `POST /hook` with a short timeout; native
`ai-memory hook --event` commands spool events locally with a **stable per-entry idempotency
key** and hand session-end delivery to a detached lock-aware `hook-drain` helper. Agent hot
paths never block on the network; saturated servers return **HTTP 429** instead of queueing
unbounded work.

On true `SessionEnd`, the server synthesizes a `sessions/<id>.md` summary page
(**rule-based, no LLM**) and opens a `Handoff` row for the next agent. One SQLite transaction
inserts the handoff, stamps the session ended, and records the covered observation count —
"so recovery never sees only half of those DB effects."

### 1.7 Retrieval

`memory_query` answers via **FTS5 + entity-match + link-neighbour RRF**; when an embedder is
configured, vector cosine over `page_embeddings` joins the same RRF. Optional
`AI_MEMORY_RERANKER=llm` pass (bounded: 1 call/query, 4 in flight, failures preserve local
order). Authority multiplier adjusts by page kind/tier/pinned. Decay model: per-tier
half-life curves, access boosts, DBSCAN cold-cluster dedup, contradiction flagging —
**zero-LLM, reversible, off-by-default**.

### 1.8 Concurrency, failure, durability

Writer actor + WAL + transactions; idempotency keys for hook events; at-least-once downstream
effects gated on a completion marker; 429 backpressure; non-overlapping scheduler ticks;
crash-convergent replays. This is genuinely hardened, production-grade work — it is the
strongest single asset in the workspace.

### 1.9 Security

`SECURITY.md`, `DATA_HANDLING.md`, `docs/security.md`, `docs/security-boundaries.md`. Every
LLM prompt treats repository text, observations, wiki pages, and prior proposals as
**untrusted data rather than instructions**, with explicit trust boundaries and delimiters
before injected handoffs and briefs. Per-project authz design; API-key auth; audit log with
`author_id` (`V16`). Gitleaks configured. A recent merge commit added "five adversarial
security-boundary regressions across handoff, page-sharing, message, and lifecycle-guard
invariants" plus a "mandatory adversarial-test rule" — security is a live discipline here.

### 1.10 The one feature closest to Director Brain: `ai-memory run`

`docs/managed-workstreams.md`: an opt-in launcher letting one logical coding session move
between Claude Code, Codex, OpenCode, Pi, Crush, Kimi Code, OMP, Grok Build, Antigravity CLI,
etc. Quit Claude, continue the same workstream in Codex, return to Claude later with native
`--resume`. On first launch it **auto-installs that harness's hooks and MCP** idempotently.

This is real cross-harness continuity — but at the **memory/session** layer, not the
**task/planning/verification** layer.

### 1.11 Limitations for Director Brain

- **No task model at all.** No tasks, no subtasks, no dependency graph, no estimation. Grep
  for `\btask\b` hits only incidental mentions.
- **No agent registry.** Agents are captured passively as session metadata, never registered,
  never heartbeated, never assigned.
- **No planner, no verification, no replanning.**
- **No checkpoint concept tied to a task.** Handoffs are session-scoped summaries, created on
  SessionEnd.
- **Capture is hook-driven and ambient** — it depends on the harness emitting hooks, not on a
  supervisor deciding to checkpoint.

---

## 2. handoff-mcp — Architecture Summary

### 2.1 What it is

handoff-mcp is an MCP server giving AI coding agents **persistent session context across
sessions**. When you close a Claude Code session and start a new one, the new session has no
idea what the previous one was doing; handoff-mcp saves session context — tasks, decisions,
blockers, file pointers — to a local `.handoff/` directory the next session loads.

It is **two things at once**:

1. A **Rust MCP server** (`src/`) — the substrate.
2. A set of **Claude Code / Codex skills and plugins** (`skills/`, `plugin/`,
   `plugin-task-loop/`, `plugin-hooks/`, `plugins/`) — the orchestration, expressed as
   markdown skill instructions and JS workflows that run *inside* the harness.

### 2.2 Data model (canonical entities)

**`TaskData`** (`src/storage/tasks.rs:25`) — the core entity:

```rust
pub struct TaskData {
    pub id: String,              // task identity — agent-independent
    pub title: String,
    pub notes: Option<String>,
    pub priority: Option<String>,
    pub created_at / updated_at / completed_at: Option<String>,
    pub labels: Vec<String>,
    pub links: Vec<String>,
    pub task_links: Vec<TaskLink>,        // typed doc/url/file/task links
    pub done_criteria: Vec<DoneCriterion>,
    pub schedule: Option<Schedule>,
    pub dependencies: Vec<String>,
    pub order: Option<u32>,
    pub assignee: Option<String>,
    pub lock: Option<TaskLock>,           // cross-process claim lease
    pub scope_paths: Vec<String>,         // file/dir scope, advisory overlap warning
    pub extra: HashMap<String, Value>,    // #[serde(flatten)]
}

pub struct TaskLock {
    pub agent_id, session_id, claimed_at, lease_expires_at: String,
    pub lease_ttl_seconds: u64,
}
```

**`SessionData`** (`src/storage/sessions.rs:25`) — rich, and notably already records project
state:

```rust
pub struct SessionData {
    pub version: u32, pub id: Option<String>, pub ended_at: Option<String>,
    pub summary: String,
    pub branch: Option<String>,          // project state observation
    pub commit: Option<String>,          // project state observation
    pub dirty_files: Vec<String>,        // project state observation
    pub decisions: Vec<Value>,
    pub blockers: Vec<String>,
    pub checklist: Vec<Value>,
    pub handoff_notes: Vec<Value>,
    pub references / context_pointers: Vec<Value>,
    pub environment: Option<Value>,
    pub timeline: Option<String>,
    pub label: Option<String>,
    pub parent_session_id: Option<String>,   // fork lineage
    pub related_task_ids: Vec<String>,
    pub agent_id: Option<String>,            // owner — but task IDs are separate
    pub worktree: Option<String>,
    pub scope: Option<String>,               // "primary" | "worktree" | "ephemeral"
}
```

**`AgentRecord`** (`src/storage/agents.rs`) — a real registry:

```rust
pub struct AgentRecord {
    pub agent_id: String,
    pub session_id: Option<String>,
    pub worktree: PathBuf,
    pub branch: Option<String>,
    pub pid: Option<u32>,
    pub registered_at: DateTime<Utc>,
    pub last_heartbeat: DateTime<Utc>,
    pub status: AgentStatus,         // Active | Stale | Disconnected
    pub claimed_tasks: Vec<String>,
    pub metadata: HashMap<String, Value>,
}
```

Heartbeat TTL 30 min → `Stale`; 60 min → `Disconnected`; GC after 7 days. Heartbeat writes
debounced to 1/min. Agent id prefers `CLAUDE_SESSION_ID`.

**`MemoryEntry`** (`src/storage/memory/model.rs`) — one file per memory under
`.handoff/memory/`, `kind ∈ {lesson, rule, convention, gotcha}`, `tags`, `keywords`
(BM25-boosted), `scope_paths`, `content_hash` (FNV-1a for dedup + re-injection tracking).
Token sets are **recomputed from text on every read so the index can never drift**.

**`EventRecord`** (`src/storage/events.rs`) — append-only JSONL at `.handoff/events.jsonl`:
`task.claimed`, `task.released`, `task.expired`. Deliberately not atomic-write (plain
`O_APPEND`) since it is append-only and high-frequency.

Plus: `DocFragment` (document management: split/frontmatter/reassemble), `Referral`,
`Assignee`, `Milestone`, `CalendarEntry`, `TimerState`.

### 2.3 Storage model

- **Per-project `.handoff/` directory of JSON/TOML files**, in the working tree:
  `tasks/`, `sessions/`, `agents/`, `docs/` (+ `docs/injected/`), `memory/`, `events.jsonl`,
  `config.toml`, `.active.json`, `version`, `timer/state`.
- **Atomic writes** (`src/storage/mod.rs:82`): temp file → `fsync` → rename. Windows retries
  the rename up to 10× with exponential backoff because a concurrent reader (the VSCode
  extension polls `.handoff/`) can make it fail transiently. POSIX rename is atomic.
- **`fs2` file locks** for cross-process mutual exclusion (claim/release, task writes).
- `src/storage/git.rs` — git operations for worktree detection (`primary` / `worktree` /
  `ephemeral` scope) and branch resolution. First-class multi-worktree support.
- **State is explicitly local.** `README.md:439`: *"Add `.handoff/` to your `.gitignore` — it
  contains local working state, not code."* There is no server, no database, no replication.

### 2.4 Session lifecycle — a real state machine

`open` → `active` (`.active.json` survives interruption) → `paused` → `closed`, with
`activate_*`, `close_*`, `pause_*`, `resume_*`, `read_sessions_by_status`, and
`update_and_close_active_session`. **Fork** (`fork_session`, sets `parent_session_id`) and
**merge** (`merge_sessions`) are implemented, with fork lineage erased on close by design.

### 2.5 MCP tool surface (~80 tools, all `handoff_*`-prefixed)

- **Session**: `handoff_init`, `handoff_start_project`, `handoff_load_context`,
  `handoff_save_context`, `handoff_import_context`, `handoff_get_session`,
  `handoff_list_sessions`, `handoff_update_session`, `handoff_fork_session`,
  `handoff_merge_sessions`
- **Task**: `handoff_list_tasks`, `handoff_get_task`, `handoff_update_task`,
  `handoff_bulk_update_tasks`, `handoff_task_checklist`, `handoff_check_criterion`
- **Ownership / assignment**: `handoff_claim_task`, `handoff_reclaim_task`,
  `handoff_release_task`, `handoff_add_assignee`, `handoff_list_assignees`,
  `handoff_update_assignee`, `handoff_remove_assignee`
- **Scheduling**: `handoff_auto_schedule`, `handoff_get_capacity`, `handoff_update_calendar`,
  `handoff_add_milestone`, `handoff_list_milestones`, `handoff_update_milestone`,
  `handoff_remove_milestone`, `handoff_log_time`, `handoff_timer_start/stop/get_time`
- **Memory**: `handoff_memory_save`, `handoff_memory_query`, `handoff_memory_delete`,
  `handoff_memory_cleanup`
- **Docs**: `handoff_doc_save/get/list/delete/import/analyze/query/graph/tree/trace/
  reassemble/update_section/verify/verify_status`
- **Observation**: `handoff_events`, `handoff_overview`, `handoff_dashboard`,
  `handoff_get_metrics`, `handoff_refer`, `handoff_get_referral`, `handoff_list_referrals`,
  `handoff_update_referral`, `handoff_get_config`, `handoff_update_config`,
  `handoff_update_labels`

### 2.6 Scheduling and dependencies

`auto_schedule.rs` implements a genuine (if simple) **topological sort** — tasks with no deps
first, then by `order`; `ready_tasks` = `todo` tasks whose every dependency is `done`, sorted
by priority (high > medium > low > unspecified) then `order`/id for determinism; due dates
propagate from dependency completion dates. This is real dependency-aware scheduling, not
just stored metadata.

`scope_paths` drives **advisory file-overlap warnings** on `handoff_claim_task` — the seed of
conflict detection. It warns; it never blocks.

### 2.7 Orchestration layer (skills + JS workflows)

`plugin-task-loop/` provides `/session-loop` (Claude) and `$handoff-session-loop` (Codex):
parallel TDD implementation, adversarial testing, Opus-style review; `/research-loop` for
multi-agent investigation. Supporting libs: `task-graph.js`, `gate-ledger.js`,
`verdict-logic.js`, `budget.js`, `codex-adapter.js`, `context-injection.js` — each with
co-located unit tests. Agent personas: `session-developer`, `session-tester`,
`session-reviewer`, `session-repairer`, `session-integration-tester`, `session-doc-
reconciler`, `research-director/investigator/drafter/verifier`.

**This orchestration is prompt-markdown and JS running inside the harness, not Rust.** It is
inherently harness-coupled (separate Claude and Codex copies, kept deliberately separate by
repo policy: "Do not make Codex features depend on Claude commands or Workflow DSL").

### 2.8 Security and reliability

`UNKNOWN_IDENTITY` sentinel is documented as *never* comparable for ownership — two
unregistered callers both resolve to it, so a plain `==` would silently release another
caller's state. That is careful, adversarial-aware engineering. Shell-script hooks exist for
Codex. `THIRD_PARTY_LICENSES.md` is maintained. npm ships prebuilt binaries per platform.

### 2.9 Limitations for Director Brain

- **No checkpoint concept.** `grep -rin "checkpoint" src plugin-task-loop skills` → **zero
  matches.** Nothing records task/branch/commit/blocker/next-action together for resumption.
- **No verification engine.** `handoff_check_criterion` is an **agent self-reported checkbox
  tick**. Nothing independently inspects git, files, or tests. A task reaches `done` because
  an agent said so.
- **No planner, no replanning, no staleness detection.**
- **Agent-driven, not supervisor-driven.** Its own `AGENTS.md`: the agent calls
  `handoff_load_context` at start and `handoff_save_context` at end. If the agent crashes
  without saving, continuity depends on `.active.json` alone.
- **Machine-local only.** `.handoff/` is gitignored. No cross-machine story.
- **JSON files, not a database.** No transactions across multiple entities, no FTS, no
  concurrent writers beyond file locks.

---

## 3. Direct Feature Comparison

| Subsystem | ai-memory 2.4.0 | handoff-mcp 0.35.1 |
|---|---|---|
| **Task model** | ❌ none | ✅ `TaskData` + dependencies + done criteria |
| **Task identity independent of agent** | n/a | ✅ `TaskData.id` ≠ `agent_id` |
| **Dependency graph** | ❌ | ✅ topological `auto_schedule` + `ready_tasks` |
| **Agent registry / heartbeat** | ❌ (passive session metadata) | ✅ `AgentRecord` + TTL/GC |
| **Assignment / claim** | ❌ | ✅ assignee + `TaskLock` lease + reclaim |
| **Session model** | ✅ SQLite `sessions` + `agent_kind` | ✅ JSON `SessionData` + fork/merge |
| **Session lifecycle** | hook-driven (start/end) | ✅ explicit state machine (open/active/paused/closed) |
| **Cross-harness session continuation** | ✅✅ `ai-memory run` workstreams | ❌ (per-session, one harness) |
| **Handoff** | ✅ typed claim-once `Handoff` row | ✅ `handoff_notes` + save/load_context |
| **Memory (long-term knowledge)** | ✅✅ markdown wiki + FTS5 + vec + decay | ✅ basic BM25 over `MemoryEntry` |
| **Retrieval quality** | ✅✅ RRF + entity + link + reranker | ✅ BM25 + keywords + scope boost |
| **Ambient lifecycle capture** | ✅✅ hooks → observations → FTS | ❌ agent must call save_context |
| **Checkpoint** | ❌ | ❌ |
| **Verification** | ❌ | ❌ (self-reported checklist only) |
| **Planner / replanning** | ❌ | ❌ |
| **Project state observation** | partial (session page metadata) | ✅ branch/commit/dirty_files in session |
| **Conflict detection** | ❌ | ⚠️ advisory `scope_paths` warning on claim |
| **Multi-agent coordination** | ✅ cross-project messaging only | ⚠️ claim leases + overlap warnings |
| **Storage** | SQLite WAL + markdown wiki | `.handoff/` JSON/TOML files |
| **Concurrency** | ✅✅ single-writer actor + idempotency keys | ⚠️ fs2 locks + atomic rename |
| **Cross-machine** | ⚠️ manual backup/rsync/git push | ❌ gitignored, local only |
| **Auth / multiuser** | ✅ API keys, per-project authz design, users | ❌ none |
| **MCP transport** | stdio + HTTP/SSE | stdio |
| **Harness coverage** | ✅✅ Claude, Codex, Cursor, Gemini, OpenCode, Devin, + more | Claude Code + Codex |
| **Orchestration loop** | ❌ | ⚠️ prompt/JS `/session-loop` (harness-coupled) |
| **LLM dependency** | optional (zero-LLM default) | none in Rust; skills invoke the harness LLM |
| **Maturity / scale** | 261k LOC, 30+ migrations, benchmarks | 26k LOC, 0.35.x |

---

## 4. Overlapping Functionality

These are the seams where the two systems collide. Director must not implement any of these.

1. **Session representation and lifecycle.** Both model sessions with agent identity, both
   synthesize session summaries, both support a "next session picks up where we left off"
   flow. Overlapping but not compatible: one is a SQLite row keyed by blob id with
   `agent_kind`; the other is a JSON file keyed by `s-YYYYMMDD-HHMMSS-NNNNNN` with a
   parent-child fork graph.
2. **Handoff.** ai-memory has a typed claim-once `Handoff` row (`open`/`accepted`/`expired`)
   with `next_steps` and `files_touched`; handoff-mcp has `handoff_save_context` /
   `handoff_load_context` producing `handoff_notes`. Same noun, different semantics —
   ai-memory's is a database record with accept-protocol state; handoff-mcp's is a document.
3. **Long-term project memory.** ai-memory's wiki pages vs handoff-mcp's `MemoryEntry`
   files. ai-memory's is strictly more capable (FTS5, vectors, decay, supersession).
4. **Event/observation log.** ai-memory's immutable sanitized JSONL observations + FTS vs
   handoff-mcp's `.handoff/events.jsonl` lease events.
5. **Agent identity.** ai-memory derives it from the harness session (`agent_kind`, session
   id); handoff-mcp has an explicit `AgentRecord` with heartbeat. Overlapping identity
   models with no shared notion.
6. **Cross-agent awareness.** ai-memory's messaging vs handoff-mcp's claim leases and
   assignees.

---

## 5. Integration Risks

**R1 — Incompatible storage substrates (the big one).** ai-memory is SQLite + a markdown
wiki in an external data dir, serialized by a single-writer actor. handoff-mcp is JSON files
inside the project tree guarded by fs2 locks. There is no shared transaction, no shared
identity, no shared locking. Any attempt to make one the store of the other's records means
either duplicating handoff-mcp's task tables into ai-memory's SQLite (reimplementing
handoff-mcp) or bolting a wiki onto JSON files (destroying ai-memory's retrieval).

**R2 — Incompatible identity models.** Session ids are blob UUIDs in one and timestamp
strings in the other; agent identity is `agent_kind` metadata in one and `AgentRecord` with
`CLAUDE_SESSION_ID` in the other. Any merge requires a global identity translation layer.

**R3 — Versioning and coupling risk.** ai-memory is 2.4.0 with 30+ SQL migrations, edition
2024, rustc 1.95, and a rapidly-evolving surface (auto-improvement, decay, managed
workstreams all recently landed). handoff-mcp is 0.35.x. Deep source coupling to either
means inheriting their churn. Neither publishes a stable ABI.

**R4 — Crate dependency is possible but unsafe for coupling.** Both are `pub` library
crates (`src/lib.rs` exists in both; ai-memory crates carry version numbers suitable for
crates.io). But depending on `ai-memory-store` internals means importing its migration
lifecycle and its writer actor; depending on `handoff-mcp` as a library means importing a
process-local file-lock model that assumes it owns `.handoff/`.

**R5 — Orchestration in handoff-mcp is harness-coupled.** The task loop lives in prompt
markdown + JS inside Claude/Codex. Director cannot reuse it without becoming
harness-specific — directly contradicting the vendor-independence requirement.

**R6 — handoff-mcp's verification is not verification.** `handoff_check_criterion` is a
self-report. Building Director's verification on it would inherit the exact failure mode the
acceptance criteria forbid ("Agent claims 'Done.' Tests fail → must not become COMPLETED").

**R7 — Neither has cross-machine state.** handoff-mcp state is gitignored by design;
ai-memory needs manual rsync/backup. The Computer A → Computer B acceptance test cannot be
satisfied by either as-is.

**R8 — Double-capture double-write.** If Director sits in front of ai-memory's MCP *and*
ai-memory's hooks still fire, the same tool call is captured twice (Director's own observation
+ ai-memory's ambient hook capture). Must be de-duplicated or the layers must divide capture
responsibility explicitly.

---

## 6. Proposed Integration Boundary

```
                    ┌─────────────────────────────────────────────┐
                    │            DIRECTOR BRAIN                    │
                    │  (owns: plan, task, assign, verify,          │
                    │   checkpoint, recover, replan, context)      │
                    │                                             │
                    │   ┌───────────────────────────────────────┐  │
                    │   │  Canonical domain model (Director's    │  │
                    │   │  OWN entities + provider TRAITS)       │  │
                    │   └───────────────┬───────────────────────┘  │
                    │                   │                          │
                    │   ┌───────────────▼───────────────────────┐  │
                    │   │  HandoffAdapter      MemoryAdapter     │  │
                    │   │  (TaskProvider,      (MemoryProvider,  │  │
                    │   │   AgentProvider,      ProjectState-     │  │
                    │   │   SessionProvider)    Provider)         │  │
                    │   └───────┬────────────────────┬───────────┘  │
                    └───────────┼────────────────────┼──────────────┘
                                │ MCP (stdio/HTTP)   │ MCP (stdio/HTTP)
                                │                    │
              ┌─────────────────▼──┐      ┌───────────▼──────────────┐
              │   handoff-mcp      │      │        ai-memory         │
              │   task/session/    │      │   wiki + memory +        │
              │   agent substrate  │      │   retrieval + hooks      │
              │   (.handoff/ JSON) │      │   (SQLite + markdown)    │
              └────────┬───────────┘      └────────────┬─────────────┘
                       │                                │
                       └────────────┬───────────────────┘
                                    │  (Director drives these)
                                    ▼
                    ┌──────────────────────────────────────┐
                    │  HARNESS LAYER: Claude Code, Codex,   │
                    │  DeepSeek, OpenCode, Goose, Cursor …  │
                    └──────────────┬───────────────────────┘
                                   │
                                 Git / Code → PROJECT
```

**Boundary rules:**

1. **Director talks to both substrates exclusively over MCP**, never by importing their
   internal storage structs. This is the single most important rule: it means Director's
   `Task` never touches `TaskData`, and Director's `Memory` never touches a wiki `Page`.
   Both substrates already speak MCP — use the boundary that already exists.
2. **Director owns the task lifecycle, both substrates own persistence of their own
   records.** Director's `Plan`, `Checkpoint`, `Verification`, `Decision`, `Blocker`,
   `Recovery`, and `RecentContext` entities live in **Director's own store** and are never
   delegated. The substrates are read/write backends for tasks, sessions, agents, memory.
3. **handoff-mcp becomes the task/session/agent substrate.** Its `TaskData`,
   `dependencies`, `AgentRecord` with heartbeat, `TaskLock` claim leases, and session state
   machine are exactly what Director needs for assignment and coordination — and they are
   already agent-independent.
4. **ai-memory becomes the memory/retrieval substrate.** Its wiki, FTS5+vector retrieval,
   ambient hook capture, typed claim-once handoffs, and `ai-memory run` cross-harness
   workstreams are exactly what Director needs for recent context, project knowledge, and
   evidence.
5. **Director's checkpoint is Director's own.** Neither substrate has one, and a checkpoint
   must record task + objective + branch + commit + changed files + tests + blockers +
   decisions + next action together — a cross-subsystem composite no single substrate can
   express. Director must own it.
6. **Director's verification is Director's own.** It must inspect git/files/tests directly,
   never trusting a self-reported criterion tick.
7. **Director owns cross-machine continuity**, because neither substrate has it.

---

## 7. What Should Be Reused / Wrapped / Kept Separate

### 7.1 Reuse as-is (via MCP, no changes to either repo)

| From | What | Why it's safe |
|---|---|---|
| handoff-mcp | task CRUD, `dependencies`, `bulk_update_tasks` | Mature, agent-independent, exactly the right granularity |
| handoff-mcp | `AgentRecord` registry + heartbeat + TTL/GC | Director's AgentRegistry maps onto it directly |
| handoff-mcp | `claim_task`/`reclaim_task`/`release_task` + `TaskLock` | Cross-process safety already solved |
| handoff-mcp | session state machine, `fork_session`/`merge_sessions` | Resume and parallel-work support already solved |
| handoff-mcp | `auto_schedule` topological ready-set | Dependency-aware scheduling already solved |
| handoff-mcp | `scope_paths` overlap warnings | Seed for Director's conflict detection |
| handoff-mcp | `events.jsonl` append-only audit | Director's audit log substrate |
| ai-memory | `memory_query` / `memory_recent` / `memory_briefing` | Retrieval is solved properly; do not rewrite |
| ai-memory | wiki `write_page`/`read_page`/`explore` | Durable project knowledge store |
| ai-memory | typed handoff `begin`/`accept`/`list` | Claim-once handoff protocol |
| ai-memory | `hook` capture + observations | Ambient activity capture for RecentContext |
| ai-memory | `ai-memory run` | Cross-harness session launching |

### 7.2 Wrap (adapter, Director owns the interface)

- `HandoffAdapter` → implements `TaskProvider`, `AgentProvider`, `SessionProvider`,
  `HandoffProvider` by calling handoff-mcp MCP tools; maps `TaskData` ↔ Director `Task`.
- `AiMemoryAdapter` → implements `MemoryProvider`, `ProjectStateProvider`,
  `EvidenceProvider` by calling ai-memory MCP tools; maps wiki `Page` ↔ Director `Memory`.
- Both adapters translate identity (Director `AgentId`/`TaskId` ↔ substrate ids) and absorb
  schema differences so Director's core never sees a foreign struct.

### 7.3 Keep separate

- handoff-mcp's `/session-loop`, `$handoff-session-loop`, `/research-loop`, and all JS
  workflow libs. They are harness-coupled prompt orchestration. Director replaces them with
  its own harness-agnostic execution loop.
- ai-memory's auto-improvement / consolidation LLM pipeline. It is a memory-maintenance
  feature, orthogonal to orchestration. Director may consume its output; it must not drive
  Director's loop.
- ai-memory's web UI and multiuser authz. Separate concern; Director fronts its own MCP.

### 7.4 Director must implement itself (no substrate covers it)

1. **Checkpoint engine** — neither has any checkpoint concept.
2. **Verification engine** — independent git/file/test inspection; substrate "verification"
   is self-report.
3. **Planner + replanner** — task decomposition grounded in observed project state.
4. **Recent context engine** — bounded, compacted activity window.
5. **Recovery + resume orchestration** — crash detection, stale-checkpoint detection,
   continuation-package assembly.
6. **Multi-agent conflict detection** — deterministic file/branch/task overlap, beyond the
   advisory warning.
7. **Cross-machine state continuity** — the Computer A → B test.
8. **Director's own persistence** — checkpoints, plans, verifications, decisions, and
   recovery packages are Director records and need Director's store.

---

## 8. Recommendation: Use / Wrap / Fork / Integrate

| Repo | Verdict | Boundary |
|---|---|---|
| **ai-memory** | **USE as an external dependency, via MCP.** Do not fork, do not copy source, do not depend on its crates. | Run it as a local server (Docker or binary) and talk to it over MCP/HTTP. Consume `memory_*` tools and hook-captured observations. Its hook layer stays the ambient capture mechanism. If Director needs an LLM-bearing summary, call `memory_consolidate`. |
| **handoff-mcp** | **USE as an external dependency, via MCP.** Do not fork, do not copy source. | Run it as a stdio MCP server per project and talk to it over MCP. Director becomes the *only* writer through its tools, so its file-lock model stays coherent. Its skills/plugins are NOT installed in Director's path — Director replaces the task loop. |
| **Both** | **Wrap behind adapters.** Director's core depends only on Director's own traits. | A `#[cfg(test)]` in-memory fake implements every provider trait, so Director's logic is testable with zero external processes. |

**Why MCP rather than library dependency:** the integration rule says prefer the boundary
that is cleanest and most stable. Both projects already expose MCP. MCP gives process
isolation (Director cannot crash the memory server, and vice versa), version independence
(upgrade ai-memory without recompiling Director), language independence (the adapter could
be reimplemented), and it forces the adapter boundary to be real rather than conventional.
Library coupling would import two incompatible storage lifecycles, two lock models, and two
migration chains into one binary — precisely the failure mode the brief warns against.

**Attribution:** both are MIT. Director will carry `THIRD_PARTY_LICENSES.md` reproducing both
MIT notices (© 2026 Fabio Akita) and will document that it communicates with both as separate
works over MCP. No source is copied.

---

## 9. Recommended Implementation Language

**Rust.** The evidence:

1. Both substrates are Rust. The adapter boundary is lowest-friction in Rust — serde types
   map directly onto both projects' JSON, and the MCP client (JSON-RPC over stdio/HTTP) is
   trivially expressed.
2. Director's core loop is a long-running, concurrent state machine with an event/log tail,
   exactly the profile where Rust's fearless concurrency and zero-cost abstraction pay off,
   and where Python's GIL and runtime cost hurt.
3. Director's own persistence needs transactions, WAL, and a single-writer pattern — the same
   design ai-memory already proved in Rust with `rusqlite`.
4. Performance class: Director is on the hot path of every agent action (observation,
   checkpoint, heartbeat). Python would add per-event overhead and GC pauses to a system
   whose whole purpose is low-latency continuity.

**Python later, opt-in and separate:** semantic analysis, plan-quality evaluation,
LLM-heavy research, eval harnesses — components where the ecosystem advantage is real and the
latency requirement is not. Never in the core loop.

**Toolchain note for this environment:** no Rust toolchain is currently installed (no
`cargo`, no `rustup`; Node v23.5.0 is present). Phase 1 requires installing rustup with
toolchain 1.95+ to match ai-memory's `rust-toolchain.toml`. This is an environment
prerequisite, not an architectural decision.

---

## 10. Proposed Director Architecture

```
director-brain/
├── crates/
│   ├── director-domain/       # Canonical entities + provider TRAITS. Zero deps on substrates.
│   ├── director-store/        # Director's own SQLite: checkpoints, plans, verifications,
│   │                          # decisions, recent context, recovery packages.
│   ├── director-adapters/     # HandoffAdapter + AiMemoryAdapter (MCP clients) + InMemory fake.
│   ├── director-context/      # RecentContext engine (bounded window + compaction).
│   ├── director-checkpoint/   # Checkpoint engine.
│   ├── director-observe/      # Project state observation (git/fs/tests) + staleness.
│   ├── director-plan/         # Planner + replanner.
│   ├── director-agents/       # Agent registry + assignment + capability matching.
│   ├── director-verify/       # Verification engine (VERIFIED/PARTIALLY/FAILED/BLOCKED).
│   ├── director-recovery/     # Recovery + resume engines, continuation packages.
│   ├── director-loop/         # The OBSERVE→PLAN→ASSIGN→MONITOR→VERIFY→REPLAN loop.
│   ├── director-mcp/          # Thin MCP handlers exposing director_* tools.
│   └── director-cli/          # Binary.
└── docs/                      # This report + phase docs.
```

Control flow (Phase 9 onwards, but the shape is fixed now):

```
OBSERVE (git/fs/tests + substrate events + agent heartbeats)
   → UNDERSTAND (project state vs last checkpoint; STATE_CHANGED?)
      → PLAN / REPLAN (grounded in observed state, never invented facts)
         → ASSIGN (capability match + claim lease via HandoffAdapter)
            → MONITOR (heartbeat TTL, lease expiry, progress reports)
               → VERIFY (independent git/file/test inspection)
                  → UPDATE STATE (Director store + substrate records)
                     → loop back, or RECOVER on failure
```

---

## 11. Recommended Phase 1 Plan

**Goal:** establish Director's canonical domain model and the provider trait boundary, with
zero coupling to either substrate and passing tests.

**Scope (Phase 1 only):**

1. Create the `director-brain` Rust workspace, edition 2021 (widest compatibility; no need
   for edition 2024's features in a domain crate), toolchain pinned via `rust-toolchain.toml`.
2. Crate `director-domain`:
   - Entities: `Project`, `Task`, `Subtask`, `Agent`, `AgentSession`, `Machine`,
     `Checkpoint`, `Action`, `Plan`, `Decision`, `Blocker`, `Handoff`, `ContextSnapshot`,
     `ProjectState`, `TaskState`, `AgentCapability`, `AgentAssignment`.
   - **Invariants encoded in types:** `TaskId` and `AgentId` are distinct newtypes so a task
     can never be confused with an agent; `TaskStatus` and `VerifyResult` are enums;
     `Assignment` links a `TaskId` to an `AgentId` but `Task` carries no agent field.
   - All identity, status, and verification enums live here so MCP handlers and adapters
     share one vocabulary.
3. Provider traits in `director-domain::providers`:
   `MemoryProvider`, `HandoffProvider`, `SessionProvider`, `TaskProvider`, `AgentProvider`,
   `ProjectStateProvider`, `ExecutionProvider` — each `async fn`, each returning Director's
   own types, each with an associated `Error`.
4. Crate `director-adapters` with an `InMemoryProvider` implementing every trait, so all
   later phases are testable without external processes.
5. Unit tests proving: task identity is stable across three different assigned agents; a
   `Task` can be reassigned without changing identity; assignment history is retained.
6. `THIRD_PARTY_LICENSES.md` with both MIT notices.
7. No MCP server, no storage, no loop yet — those are later phases.

**Out of scope for Phase 1:** any adapter that actually talks to a substrate (Phase 2/3),
any persistence (the store crate comes with checkpoints in Phase 5), any planner.

**Definition of done:** `cargo test` passes, `cargo clippy` is clean, the workspace contains
no reference to `handoff` or `ai_memory` outside `director-adapters` (enforced by a test).

---

## 12. Known Limitations of This Audit

- Both repositories were audited from source at the checked-out commit; neither was built or
  run (no Rust toolchain present in this environment).
- handoff-mcp's JS workflow libraries (`task-graph.js`, `gate-ledger.js`, `verdict-logic.js`)
  were inventoried and their purpose established, but not line-reviewed in depth. They are
  out of scope for reuse regardless — they are harness-coupled.
- ai-memory's `ai-memory-workstream` crate and the wiki's split/reassemble internals were
  surveyed at the API level, not deeply read.
- Runtime behavior (actual MCP round-trips, hook capture under load, scheduler behavior under
  contention) is unverified. Claims above are from source and the projects' own docs.
- The `HandoffAdapter` / `AiMemoryAdapter` mappings are designed but unproven against live
  servers — they will be validated in Phases 2 and 3.
