# Orqyn

A vendor-independent orchestration brain for multi-agent software work.

Orqyn sits **above** the coding agents (Claude Code, Codex, DeepSeek,
OpenCode, Goose, Cursor, …) and **beside** the memory/tooling substrates
([handoff-mcp](https://github.com/alphaelements/handoff-mcp) and
[ai-memory](https://github.com/akitaonrails/ai-memory)). It owns the parts
neither of those systems has: planning, assignment, checkpointing, independent
verification, recovery, and replanning.

Orqyn talks to both substrates **exclusively over MCP**. It never imports
their internal structs, never forks their source, and never depends on their
crates. A boundary test in this repo enforces that — see
[The boundary is a test, not a convention](#the-boundary-is-a-test-not-a-convention).

> **Status: Phases 1–3, 5 complete; Phase 6 underway (OBSERVE, PLAN, ASSIGN,
> MONITOR, and VERIFY landed).** The canonical domain model, the provider trait
> boundary, both substrate adapters, Orqyn's own persistent store, and the
> first five steps of the control loop are in place, with zero substrate
> coupling and a passing test suite (404 tests).
>
> - **Phase 1** — the domain model and the seven provider traits it depends on,
>   plus an in-memory implementor of every one of them.
> - **Phase 2** — the git observation layer (a git2-backed observer, a
>   file-backed store, and the composing service) and the `HandoffAdapter`, a
>   live `TaskProvider`/`AgentProvider`/`SessionProvider` over handoff-mcp on
>   stdio JSON-RPC.
> - **Phase 3** — the `AiMemoryAdapter`, a live `MemoryProvider` over
>   ai-memory. Orqyn's long-term memory is now a substrate it reuses rather
>   than reimplements: FTS5, entity, and graph retrieval with decay are already
>   solved well there.
> - **Phase 5** — `director-store`, a SQLite store for the entities no
>   substrate has. Checkpoints, assignments, plans, and decisions now survive a
>   restart, superseded rather than deleted. The task table deliberately names
>   no agent, and "at most one active assignment per task" is a partial unique
>   index in the schema, not a convention. (There is no Phase 4; the roadmap
>   skips it.)
> - **Phase 6** — `director-app`, the crate that drives the loop
>   `OBSERVE → PLAN → ASSIGN → MONITOR → VERIFY → REPLAN`. Its first five steps
>   are landed. OBSERVE: an observed repository becomes normalized belief in
>   Orqyn's store. PLAN: a stated objective and task decomposition are validated
>   — no self-dependencies, no edges to tasks outside the plan, no cycles —
>   ordered deterministically, and persisted as the project's active plan in one
>   transaction, superseding whatever it was executing before. The decomposition
>   itself arrives from the caller; PLAN makes it real, ordered, and durable
>   rather than inventing it. ASSIGN: a task the active plan makes eligible is
>   handed to a named agent — the handoff, the task's move to `in_progress`, and
>   the status transition recording it are one transaction, so an assigned task
>   is never left un-started. Which agent gets which task arrives from the
>   caller too; `ready_tasks` is the read a scheduler will drive off. MONITOR:
>   every task in flight is surveyed, its holder's lease is judged from
>   heartbeat age, and a holder past the stale window loses the lease — the
>   tenure is retained as `lease_expired`, its session is closed as `vanished`,
>   the agent is recorded as disconnected, and the task goes back to `todo` where
>   the next ASSIGN can hand it out again. It also takes the holder's reports
>   about the work: an acknowledged session is recorded when the agent starts,
>   and a completion report ends the tenure as `work_complete` and moves the task
>   to `verification_pending` — never to `done`. A quiet agent is reported and
>   left alone; a tenantless in-progress task is reported and left for REPLAN.
>   VERIFY: every task awaiting a verdict is judged by checks Orqyn runs itself
>   through its execution provider, reading exit codes the agent cannot influence
>   — a task reaches `done` only because a check actually passed. A check that
>   fails moves the task to `failed` for REPLAN to act on; a check that cannot run
>   at all leaves the task awaiting verification, because a broken environment is
>   not a verdict on someone's work. REPLAN is the remaining step, built last.
>
> There is no MCP server of Orqyn's own yet and only the first five steps of
> the loop are wired in — those are later phases, and their absence here is
> deliberate.

---

## The two invariants the whole model exists to express

### 1. Task identity is independent of agent identity

A task outlives every agent that works on it. This is enforced in the types,
not in convention:

- `TaskId` and `AgentId` are distinct newtypes. Neither can be constructed from
  the other, so a task id can never be passed where an agent id is expected.
- `Task` has **no agent field**. Assignment is a separate, first-class entity,
  `AgentAssignment`, with its own history. Reassigning a task creates a new
  assignment record and changes nothing about the task itself.

Three agents can work a task in turn and the task's identity is byte-identical
before, during, and after — while the full tenure history remains recoverable.

### 2. Nothing self-reports completion

`TaskStatus::Done` is **not reachable** from an agent's report. There is no
provider method an agent can call that completes a task. Only Orqyn's own
verification engine — which inspects git, files, and test exit codes itself —
can do that.

This is the guardrail behind the acceptance criterion: *"Agent claims 'Done.'
Tests fail → must not become COMPLETED."* Both substrates were audited and
neither satisfies it: handoff-mcp's `handoff_check_criterion` is a self-reported
checkbox tick, and ai-memory has no task model at all.

---

## Repository layout

```
orqyn/
├── crates/
│   ├── director-domain/     # The vocabulary: every entity, identity, status
│   │                        # enum, and provider trait. Knows no substrate.
│   ├── director-adapters/   # InMemoryProvider (all traits, in-process),
│   │                        # LocalExecutor (real command execution), the
│   │                        # git observation layer, and the handoff-mcp
│   │                        # adapter. The ONLY crate allowed to name a
│   │                        # substrate.
│   ├── director-store/      # SQLite store for Orqyn's own entities. Owns
│   │                        # its schema; depends only on director-domain.
│   └── director-app/        # The control loop. Composes the layers into
│                            # steps; OBSERVE, PLAN, and ASSIGN are wired in.
├── docs/
│   ├── PHASE0-FORENSICS.md  # Read-only audit of both upstream repos:
│   │                        # data models, ~30 vs ~80 MCP tools, feature
│   │                        # comparison, integration risks, boundary design.
│   ├── PHASE1-DOMAIN.md     # The model and the boundary.
│   ├── PHASE2-ADAPTERS.md   # The HandoffAdapter over handoff-mcp on stdio.
│   ├── PHASE2-OBSERVATION.md # The git observation layer.
│   ├── PHASE3-MEMORY.md     # The AiMemoryAdapter over ai-memory.
│   ├── PHASE5-STORE.md      # The SQLite store and the invariants it holds.
│   └── PHASE6-APP.md        # The control loop, and its first step.
├── Cargo.toml               # Workspace manifest.
├── rust-toolchain.toml      # Pinned: stable-x86_64-pc-windows-gnu.
└── THIRD_PARTY_LICENSES.md  # MIT notices for both substrates (© 2026 Fabio Akita).
```

### `director-domain` — the vocabulary

Every entity Orqyn speaks, with invariants encoded in types rather than in
prose.

| Module | Entities | Invariant it enforces |
|---|---|---|
| `ids` | 15 identity newtypes + `IdGenerator` | Task identity ≠ agent identity. Distinct, sealed, not interchangeable. |
| `task` | `Task`, `Subtask`, `TaskStatus`, `Priority`, `Complexity`, `ExpectedOutput` | A task has no agent field. `Done` is unreachable by agent report. Readiness is a pure function over dependency statuses. |
| `agent` | `Agent`, `Machine`, `AgentStatus`, `Harness` | Agents are replaceable, heartbeated, thin. |
| `assignment` | `AgentAssignment`, `AssignmentStatus`, `ReleaseReason` | Reassignment is a new record, so tenure history accumulates instead of being overwritten. |
| `capability` | `Capability` | Assignment filter, not a skills ontology. Only a `Human` satisfies a `Human` requirement. |
| `session` | `AgentSession`, `SessionStatus`, `SessionEnd` | Sessions are evidence trails, not the work. `ended_uncleanly()` is what recovery keys on. |
| `state` | `ProjectState`, `StateComparison`, `TestResults` | Observed, never remembered. `StateComparison` is the resume guard. |
| `checkpoint` | `Checkpoint`, `ContinuationPackage`, `ResumeStatus` | Neither substrate has this. Handles the rebase case (`CommitGone`), not just a head advance. |
| `context` | `RecentContext`, `ContextSnapshot` | The window is bounded at all times. Compaction is lossy-but-accountable. |
| `action` | `Action`, `ActionKind` | Actions are observed, not self-reported. |
| `plan` | `Plan`, `PlanStatus` | Plans supersede, they are not edited. The old plan is retained as audit trail. |
| `decision` | `Decision`, `DecisionStatus` | Decisions carry rationale and rejected alternatives, and can be reversed. |
| `blocker` | `Blocker`, `BlockerKind`, `BlockerStatus` | "Still blocked after three replans" is a reportable condition. `Obsolete` ≠ `Resolved`. |
| `graph` | `PlanNode`, `GraphError`, `order`, `validate_shape` | A plan's tasks order deterministically: topological, with ties broken by the caller's listing order — never hashmap order or the clock. Self-dependencies, edges to tasks outside the plan, and cycles are rejected and *named*, before anything is persisted. |
| `handoff` | `Handoff`, `HandoffState`, `HandoffError` | Claim-once: only the addressed agent may accept, and only while open. |
| `project` | `Project`, `DefaultBranch` | A root, not a container — tasks are referenced by id, never nested. |
| `repository` | `Repository`, `ProjectStateSnapshot`, `WorktreeState`, `ObservationEvent`, `SyncResult` | What Orqyn learned by looking at git, and how it tells one observation from the next. Neither substrate has this. |
| `providers` | 7 provider traits + `Provider`, `ProviderError`, `Memory`, `MemoryQuery`, `CommandSpec`, `CommandOutcome` | The trait boundary. |

### `providers` — the boundary

Seven traits, all `async`, all returning Orqyn's own types, each with an
associated `Error`:

| Trait | Purpose |
|---|---|
| `TaskProvider` | CRUD, status, `ready_tasks`, dependency edges |
| `AgentProvider` | Registry, heartbeat, availability |
| `SessionProvider` | Lifecycle, per-task history, fork |
| `HandoffProvider` | Orqyn's claim-once transfer |
| `MemoryProvider` | Durable knowledge, query, recency |
| `ProjectStateProvider` | Observed git/filesystem state |
| `ExecutionProvider` | Run real commands; Orqyn reads the exit code itself |

Four rules are baked in:

1. Methods are `async` — every real substrate is an I/O boundary.
2. Inputs and outputs are Orqyn's types, never a substrate's struct.
3. Each trait has an associated `Error`, so a substrate's failure vocabulary
   cannot become Orqyn's.
4. **Nothing here completes a task.**

Deliberately absent from the traits: checkpoint, plan, decision, blocker,
verification, and assignment storage. Those are Orqyn-owned entities in
Orqyn's own store. Exposing them as provider traits would invite a substrate
to become authoritative over Orqyn's own state.

### `director-adapters` — the proof

- **`InMemoryProvider`** — one struct implementing *every* provider trait
  against in-process `HashMap`s. It is faithful to the trait *contracts*
  (claim-once handoffs, unique ids, dependency-aware readiness) so a test
  passing against it is evidence about the loop, not a tautology. This is why
  Orqyn's loop can be developed and tested with zero external processes.
- **`LocalExecutor`** — a real `ExecutionProvider` that runs commands via
  `std::process` with a timeout. It is the reference local executor and the
  fallback the verification engine will use when a substrate cannot execute
  commands.
- **The git observation layer** — `crates/director-adapters/src/git/`, the
  Phase 2 `ProjectStateProvider` for a local working tree. Three pieces, each
  separately testable:
  - **`observer`** — a git2-backed read-only lens over a working tree: HEAD and
    branch state, the commit range between two observations, working-tree
    status with rename and copy detection, and diff contents. It never opens a
    repository for writing.
  - **`store`** — a JSON file store keyed by repository, holding the last
    observed state so a subsequent observation can be compared against it
    rather than against nothing.
  - **`service`** — composes the two into `GitService`, producing
    `ProjectStateSnapshot` and the `ObservationEvent`s that changed since last
    time. This is the deterministic change detection the verification engine
    will key on: the same walk of the same repository yields the same events.

  `CommitInfo` was widened in the same step to the full canonical commit model
  — parents, committer, and the complete message body — with `serde` defaults
  so checkpoints written by Phase 1 stay readable. A commit message is recorded
  as **evidence, not verified truth**: no task is ever completed because a
  message says "done".

The handoff-mcp adapter is built. `crates/director-adapters/src/handoff/`
holds four layers:

- **`wire`** — serde mirrors of handoff-mcp's JSON shapes. Deliberately not the
  domain types and unable to become them, so every schema difference lives in
  one place.
- **`transport`** — a stdio JSON-RPC client. Orqyn spawns the server as a
  child process and speaks line-delimited JSON-RPC 2.0 over stdin/stdout. It
  never links the substrate's code; it crosses a process boundary.
- **`mapping`** — the bidirectional translation, and the layer that earns its
  keep. The two models are not isomorphic: task statuses (6 vs 8 states),
  priorities, and — critically — the meaning of `done`. handoff-mcp's `done` is
  an agent self-report, so it becomes `VerificationPending`, not `Done`, unless
  Orqyn itself produced it (a marker stashed in the substrate's `extra`
  map). That is the acceptance criterion "agent claims done, tests fail → must
  not become COMPLETED", enforced at the boundary.
- **`adapter`** — [`HandoffAdapter`], composing the three above into live
  `TaskProvider`, `AgentProvider`, and `SessionProvider` over any
  `HandoffWire` connection. It holds the one piece of state none of those
  layers can carry: the `trusted_done_ids` set of task ids Orqyn itself
  completed, which is the only thing that lets a substrate `done` come back as
  `Done` instead of `VerificationPending`.

The `HandoffAdapter` composes these into live `TaskProvider`,
`AgentProvider`, and `SessionProvider` implementations. Its counterpart for
memory, `AiMemoryAdapter`, does the same for `MemoryProvider` over ai-memory —
three modules apiece (`wire`, `transport`, `adapter`) so the two substrates read
as one pattern. Both are landed, and both are driven by a one-method wire trait
so their mapping logic is unit-testable without a child process, with `#[ignore]`
live suites that verify the wire mirrors against the real servers.

Both adapters exist to make the substrate's model honest in Orqyn's
vocabulary — the ai-memory one, for instance, keeps the two meanings of a
`rank` field separate: a relevance score from `memory_query` and a change time
in microseconds from `memory_recent`.

---

## The boundary is a test, not a convention

`crates/director-adapters/tests/boundary.rs` walks the workspace and **fails
the build** if:

- any `.rs` file outside `director-adapters` contains the identifiers
  `handoff_mcp` or `ai_memory`, or
- any `Cargo.toml` outside `director-adapters` declares a dependency whose name
  contains `handoff` or `ai-memory`.

The test looks for the identifiers a `use` statement would need — not for
English. `director-domain`'s doc comments name both substrates, because
explaining *why* Orqyn's model differs from `TaskData` is the design
rationale for avoiding it. That is the opposite of coupling.

---

## Architecture

```
                    ┌─────────────────────────────────────────────┐
                    │            ORQYN                             │
                    │  owns: plan, task, assign, verify,          │
                    │   checkpoint, recover, replan, context       │
                    │                                             │
                    │   ┌───────────────────────────────────────┐  │
                    │   │  Canonical domain model (Orqyn's       │  │
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
                                    │  (Orqyn drives these)
                                    ▼
                    ┌──────────────────────────────────────┐
                    │  HARNESS LAYER: Claude Code, Codex,   │
                    │  DeepSeek, OpenCode, Goose, Cursor …  │
                    └──────────────┬───────────────────────┘
                                   │
                                 Git / Code → PROJECT
```

Orqyn's control loop (the shape is fixed; OBSERVE, PLAN, ASSIGN, MONITOR, and
VERIFY are built, the remaining steps are not):

```
OBSERVE (git/fs/tests + substrate events + agent heartbeats)
   → UNDERSTAND (project state vs last checkpoint; STATE_CHANGED?)
      → PLAN / REPLAN (grounded in observed state, never invented facts)
         → ASSIGN (capability match + claim lease via HandoffAdapter)
            → MONITOR (heartbeat TTL, lease expiry, progress reports) ✅
               → VERIFY (independent git/file/test inspection) ✅
                  → UPDATE STATE (Orqyn store + substrate records)
                     → loop back, or RECOVER on failure
```

Boundary rules, in full, are in [`docs/PHASE0-FORENSICS.md`](docs/PHASE0-FORENSICS.md)
§6. The short version: Orqyn owns the task lifecycle and everything
upstream of it; the substrates are read/write backends for tasks, sessions,
agents, and memory. handoff-mcp becomes the task/session/agent substrate;
ai-memory becomes the memory/retrieval substrate. Orqyn's checkpoint and
verification are its own because neither substrate has them.

---

## Requirements

- Rust, pinned by `rust-toolchain.toml` to `stable-x86_64-pc-windows-gnu`.
- A C toolchain capable of linking test binaries (a portable WinLibs
  GCC/binutils on `PATH` is what this machine uses).

## Building and testing

```sh
cargo build
cargo test --workspace
```

> **If this project directory is inside OneDrive,** its sync engine takes write
> locks on `target/` and breaks builds intermittently. Redirect the target
> directory off OneDrive:
>
> ```sh
> export CARGO_TARGET_DIR="$HOME/.orqyn-target"
> cargo test --workspace
> ```
>
> This is a developer-machine workaround, not a committed repo setting — a
> repo-level `target-dir` would be machine-specific.

## What the tests prove

Unit tests are co-located with each entity and exercise the invariants rather
than the plumbing:

- Task identity is stable across three assigned agents; reassignment preserves
  task fields; assignment history is retained after release.
- `TaskStatus::Done` is unreachable by agent report.
- A failed dependency blocks readiness.
- The recent-context window never exceeds its bound.
- A handoff cannot be accepted twice or by the wrong agent.
- A memory search returns full page bodies, not `<mark>`-tagged FTS5 fragments,
  and reports a change time only on the one read path where the substrate
  actually sends one.
- `StateComparison` distinguishes a head advance from a rebase.
- Every public entity round-trips through serde — because everything Orqyn
  persists crosses a serialization boundary sooner or later.
- A project is never left with two active plans: activation supersedes the
  sitting plan in one transaction, and a second activation attempt against the
  partial unique index is an error rather than a silent second authoritative
  plan. A superseded plan survives, linked both ways.
- A reversed decision is marked, not deleted, and cannot be reversed twice.
- A stale write is rejected rather than silently overwriting a newer one.
- A plan's dependency graph orders deterministically: the same spec always
  yields the same sequence across repeated runs, and a self-dependency, an edge
  to a task outside the plan, or a cycle is reported with the task ids involved
  rather than as a generic "invalid graph".
- A plan and every task it decomposes into are written and activated in one
  transaction, so a partially created plan is impossible; activating it
  supersedes the sitting plan in the same transaction, and a duplicate plan id
  or a task with no title is refused before anything is persisted.
- No crate outside `director-adapters` references a substrate.
- Every source file under `src/` is reachable from the module tree — an
  undeclared `.rs` file is invisible to `cargo`, so a file written but never
  wired in compiles nowhere and its tests never run. The check is a test for
  the same reason the substrate boundary is.

---

## What Orqyn deliberately does not do yet

- **No decomposition.** The PLAN step is landed: it validates a stated
  decomposition — refusing cycles, dangling edges, and self-dependencies —
  orders it deterministically, and persists it as the active plan in one
  transaction. What it does not do is decide *what* the tasks should be. The
  judgment that "add authentication" decomposes into model → endpoints →
  middleware → tests is a reasoning step and arrives from the caller as
  `TaskSpec`s; PLAN makes it real, ordered, and durable rather than inventing
  it. A step that writes its own decomposition is a later phase.
- **No scheduling.** The ASSIGN step is landed: it hands a task to a named agent
  — refusing a task the active plan does not name, a task whose dependencies
  are not finished, a task an agent already holds, and an agent that is
  unavailable or lacks a required capability — and commits the handoff and the
  task's start in one transaction. What it does not do is decide *which* agent
  gets *which* task. Matching tasks to agents by capability and load is a
  scheduling judgment and arrives from the caller; `ready_tasks` reports the
  work that can be handed out right now, and a step that assigns on its own is
  a later phase.
- **No verification engine.** The loop's VERIFY step holds the invariant —
  nothing self-reports completion, and a task reaches `done` only because a
  check Orqyn ran itself passed — but the checks it runs are the machine-checkable
  forms a plan already named. The engine that *gathers* its own evidence —
  inspecting a git diff, asserting a file is present, orchestrating a test
  suite — is a later phase.
- **No MCP server, no binary.** Orqyn's own tool surface and the process that
  runs the loop unattended are later phases. Until then, the library's tests
  are its caller.

## Roadmap

| Phase | Content |
|---|---|
| **0** ✅ | Repository forensics: read-only audit of both substrates. |
| **1** ✅ | Canonical domain model + provider trait boundary, zero substrate coupling. |
| **2** ✅ | Git observation layer (observer, store, service, `ProjectStateSnapshot`) + `HandoffAdapter`: handoff-mcp client implementing `TaskProvider`/`AgentProvider`/`SessionProvider`. |
| **3** ✅ | `AiMemoryAdapter` — ai-memory client implementing `MemoryProvider`. |
| **5** ✅ | `director-store` — Orqyn's own SQLite: checkpoints, plans, verifications, decisions, recent context, recovery packages. |
| **6** ⧗ | `director-app` — the control loop. OBSERVE, PLAN, ASSIGN, MONITOR, and VERIFY are landed and tested (git layer composed with the store; plan, tasks, and activation in one transaction; a task handed to an agent and started in one transaction; a lease reclaimed from an agent whose heartbeat went quiet, and the session that held it recorded when the agent acknowledged the work; a completion report ending the tenure and moving the task to `verification_pending`; a task judged `done` or `failed` by checks Orqyn runs itself), and the dependency-graph validation PLAN builds on is landed in `director-domain`; REPLAN is built last. |
| 10 | The verification engine. |
| — | The loop's remaining step, built last: REPLAN. |

---

## Attribution

Orqyn communicates with
[handoff-mcp](https://github.com/alphaelements/handoff-mcp) and
[ai-memory](https://github.com/akitaonrails/ai-memory) as separate works over
MCP. No source is copied from either. Both are MIT (© 2026 Fabio Akita); their
notices are reproduced in [`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md).

## License

MIT — see [`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md).
