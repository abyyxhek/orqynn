# Phase 5 — The Store: Director's Own Persistence

> **Status: complete.** The `director-store` crate and the domain's `Store` trait
> layer are built, and all four gates are green.

## Goal

Phases 1–3 gave Director a vocabulary and a live read on both substrates.
Nothing Director *owned* was persisted: a checkpoint, a plan, a decision, an
assignment, a verification result existed for as long as the process did, and
not one second longer. Phase 5 fixes that.

The goal is a store for Director's own entities — the ones no substrate has,
and the ones that must never be delegated to a provider trait. Two boundaries
meet here, and they are opposite in kind:

- `director-adapters` talks to handoff-mcp and ai-memory **over MCP**. Those
  servers own their schemas; Director asks and receives.
- `director-store` talks to a `.db` file Director **owns**. Nobody else writes
  it, nobody else's schema constrains it, and it is still readable when both
  substrates are down.

The domain defines what "the store" *means* — `director_domain::Store`, the
eight repository traits, `StoreError`, `StoredProjectState`, `ProviderSync`.
This crate is what those traits are *made of*. Nothing above it ever learns that
SQLite is involved: a caller matching on
`StoreError::StateVersionConflict` retries without importing a single rusqlite
type.

## What the schema owns, and what it refuses to own

The migration files are the only place tables are created; the application
never issues DDL at runtime. The schema holds:

- **Director-owned entities** — checkpoints, assignments, normalized project
  state, provider sync metadata, append-only status history.
- **The substrate-facing entities Director needs as anchors** — projects,
  tasks, agents, sessions — stored because Director must reason about them when
  the substrate is unreachable, and because their relationships (a task's
  assignment history, a session's lineage) are Director's own conclusions.

What it pointedly does *not* hold: a copy of either substrate's tables. Copying
a provider's schema here is how a store starts being coupled to a substrate.
`ProviderSync` is a pointer and a status per entity per provider — never a
replica. If handoff-mcp changes its schema tomorrow, Director's store does not
need a migration.

## The three invariants held in storage, not in convention

### 1. Task identity is independent of agent identity

The `tasks` table has **no current_agent_id column** — a test asserts this
directly against the schema. Assignment is the `agent_assignments` table, a
first-class entity with its own history. Three agents can work a task in turn
and the task row is byte-identical before, during, and after.

### 2. Nothing is deleted

Releasing an assignment *marks* the row `released`; it never deletes it.
Superseding a checkpoint marks the old one `superseded`. Status changes append
to `task_status_history`. Reassigning TASK-42 from one agent to another leaves
the first tenure intact and queryable — which is what makes "who worked on this
task, and why did each one stop" an ordinary query rather than an
archaeological dig.

### 3. At most one active assignment, one current checkpoint, per task

Enforced by **partial unique indexes** in the schema, not by check-then-act
code. A concurrent double-assign is rejected by the database itself, which is
the only place a rule like this can survive a race.

## What verification caught

The store was written in bulk before it was ever compiled — the mistake this
project keeps making — and arrived carrying a whole phase's worth of errors.
The gates found them; one of them was worth more than all the others.

**The partial unique indexes were inert.**

The rule "one active assignment per task" is a partial index:

```sql
CREATE UNIQUE INDEX idx_assignments_active_per_task
    ON agent_assignments(task_id) WHERE status = 'active';
```

Status enums are written as JSON text by every repository
(`json::to_json(&AssignmentStatus::Active)`), and a snake_case unit variant
serializes to the **quoted** string `"active"` — eight characters, including
the double quotes. The index predicate compared against the bare six-character
literal `active`. It matched nothing. The index existed, was documented as the
load-bearing invariant, and enforced nothing at all: two assignments could go
active on the same task and the store would have let it stand.

The checkpoint code already had a comment describing this exact trap —
"a bare `'current'` in the predicate never matches a column holding
`"current"`" — written for the supersession `UPDATE` but never applied to the
schema two feet away. The comment was correct about the code and silent about
the index.

The `tests/store.rs` integration suite caught it, because it exercises the
store through the repository the way a caller does. The schema-level unit
tests in `migrations.rs` did **not** catch it, because they inserted rows with
bare `'active'` literals — testing a value no repository call ever stores. The
two suites contradicted each other, and the integration test was the one
telling the truth. Both index predicates now compare against the JSON-encoded
form, and the schema-level tests insert in the real encoding so they test the
rule the store actually relies on.

This is the fourth confirmation of the standing lesson: source reading tells
you what you *intended*; only running the tool tells you what you *have*. An
invariant expressed in a comment and a schema file is a claim. A failing test
is a fact.

## Evidence

### Gates

All four are clean:

```sh
cargo fmt --all -- --check                         # clean
cargo clippy --workspace --all-targets -- -D warnings   # clean
cargo test --workspace                             # 268 tests, 0 failures
cargo build --workspace                            # clean
```

### Test counts and what each is evidence of

| Tests | About |
|---|---|
| 145 `director-domain` unit | Entity invariants from Phase 1, plus the `store.rs` trait layer this phase added. |
| 77 `director-adapters` unit | Unchanged from Phase 3; the adapters are untouched by this phase. |
| 22 `tests/store.rs` | Round trips for every entity through the repositories, plus the invariants: stale-version conflicts, FK refusal, duplicate rejection, the two partial-index rules. |
| 19 `director-store` unit | The connection pool, the PRAGMAs, the migration runner, and the two uniqueness rules at the schema level. |
| 3 `tests/boundary.rs` | The substrate-coupling rule: nothing outside `director-adapters` may name a substrate. `director-store` depends only on `director-domain` and passes. |
| 8 + 5 live, `#[ignore]` | The two substrate suites, re-run and still green after this phase touched nothing they depend on. |
| 2 doc-tests | Opening a store and the id generator. |

The live suites need the built server binaries:

```sh
AI_MEMORY_BINARY=/c/Users/ASUS/.aimemory-target/release/ai-memory.exe \
  cargo test -p director-adapters --test aimemory_live -- --ignored --nocapture

HANDOFF_BINARY=/c/Users/ASUS/.handoff-target/release/handoff-mcp.exe \
  cargo test -p director-adapters --test handoff_live -- --ignored --nocapture
```

## Definition of done — status

- [x] `director-domain::Store` — the marker supertrait, eight repository traits,
      `StoreError`, `StoredProjectState`, `ProviderSync`.
- [x] `director-store` crate: connection pool, PRAGMA configuration, migration
      runner, eight `Sqlite*Repository` implementations.
- [x] The three storage invariants are enforced by the schema and asserted by
      tests, not by convention.
- [x] Migration bootstrap is re-runnable on every connection without error.
- [x] All four gates clean; boundary test passes with the new crate.

## What Phase 5 deliberately does not do

- **No callers yet.** The store's operations are built ahead of the loop that
  will use them. A few helpers — `assign_task`, `set_current_task`,
  `find_session`, the repository helpers — are written and tested but not
  reached through the `Store` aggregate, so `dead_code` is allowed crate-wide
  until the wiring catches up. That allow is the next thing to delete.
- **No Phase 4.** The roadmap jumps 3 → 5; there is no `PHASE4` document and no
  Phase 4 work in this tree.
- **No MCP server of Director's own**, and no loop. Persistence exists; the
  thing that persists does not yet exist.
