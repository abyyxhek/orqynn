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

> **Status: Phases 1–3, 5, and 6 complete, Phase 7's scheduler landed, and
> Phase 10's verification record layer in place — all six steps of the control
> loop (OBSERVE, PLAN, ASSIGN, MONITOR, VERIFY, REPLAN) plus the SCHEDULE step,
> and every judgment Orqyn passes is now a durable, append-only record.**
> The canonical domain model, the provider trait boundary, both substrate
> adapters, Orqyn's own persistent store, and the whole control loop are in
> place, with zero substrate coupling and a passing test suite (494 tests).
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
>   `OBSERVE → PLAN → ASSIGN → MONITOR → VERIFY → REPLAN`. All six steps are
>   landed. OBSERVE: an observed repository becomes normalized belief in
>   Orqyn's store. PLAN: a stated objective and task decomposition are validated
>   — no self-dependencies, no edges to tasks outside the plan, no cycles —
>   ordered deterministically, and persisted as the project's active plan in one
>   transaction, superseding whatever it was executing before. The decomposition
>   itself arrives from the caller; PLAN makes it real, ordered, and durable
>   rather than inventing it. ASSIGN: a task the active plan makes eligible is
>   handed to a named agent — the handoff, the task's move to `in_progress`, and
>   the status transition recording it are one transaction, so an assigned task
>   is never left un-started. Which agent gets which task arrives from the
>   caller too; `ready_tasks` is the read the SCHEDULE step now drives off. MONITOR:
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
>   not a verdict on someone's work. REPLAN: the step that answers for the states
>   the loop's own verdicts leave behind. It surveys the project for work nothing
>   is progressing — a task VERIFY judged `failed`, a task `in_progress` with no
>   tenant, a task whose dependency reached a dead end and so can never become
>   ready — and applies a decision the caller supplies for each: retry it, rework
>   it with an amended contract, or cancel it with the reason recorded as a
>   decision that outlives the task. Which remediation applies is a reasoning
>   step and arrives from the caller, exactly as PLAN's decomposition does; a
>   round that rejects one decision applies none, and cancelling a task reports
>   the dependents it strands rather than writing them, because the scheduler
>   will never hand out a task whose dependency is not `done`.
> - **Phase 7** — the scheduler. SCHEDULE decides which ready task goes to which
>   available agent, so a loop tick no longer needs somebody to name every
>   assignment. It composes ASSIGN rather than bypassing it: the round picks a
>   pairing from `ready_tasks` and the agent registry, and applies each one
>   through ASSIGN, which still owns the handoff's legality. The policy is four
>   rules — eligibility (can accept work, not already showing a current task,
>   every required capability), fresh eyes (an agent who has never held the task
>   outranks one who released it, with the prior holder as a fallback rather than
>   a prohibition), specialist-first (fewest declared capabilities, so
>   generalists stay free for the tasks only they can cover), and a deterministic
>   id tie-break. One task per agent per round, and work no available agent can
>   do is reported rather than silently left `todo` — the staffing signal that
>   makes the step worth having over calling `assign` by hand.
> - **Phase 10 (record layer)** — the durable half of the verification engine.
>   A judgment Orqyn passes is no longer ephemeral: `Verification` records one
>   act of judging a task — the probes it asked, the evidence each gathered, the
>   verdict it reached, and the commit the evidence was gathered against — and
>   `director-store` keeps it as append-only history. Re-judging a task writes a
>   new row with its own id rather than editing the old one, so "what did Orqyn
>   believe about this task on Tuesday, and what changed its mind on Wednesday"
>   is an ordinary query. `Store::apply_verification` lands a verdict and its
>   consequences in one transaction: the verification row, the task's status
>   move, and the transition history recording it, with the destination derived
>   from the verdict so the store and the step cannot disagree about what a
>   `Passed` means. An `Unverifiable` verdict still records that Orqyn looked,
>   and moves nothing — which is what makes the task safe to survey again next
>   tick. Four probe kinds are modelled (`Command`, `TestSuite`, `Diff`, `File`),
>   and `EvidenceStatus` keeps the asymmetry that makes the model honest: only
>   `Passed` and `Failed` decide, `Unverifiable` is a property of the
>   environment rather than of the work, and `Observed` evidence is reported and
>   then deliberately ignored, because a diff is circumstantial. What is *not*
>   here yet is the engine that runs the probes — no `director-app` code gathers
>   evidence yet, so Phase 6's check-based `verify` step still holds the
>   invariant on its own.
>
> There is no MCP server of Orqyn's own yet and no process drives the loop
> unattended — the steps are library functions, and their caller is the
> tests. Those are later phases, and their absence here is deliberate.

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
VERIFY step can do that: it runs the machine-checkable commands a plan named
for the task's expected outputs, and reads their exit codes and their output
itself — the agent whose work is being judged never gets to report the outcome,
and the provider that runs a command never gets to interpret it.

What has *not* landed is the engine that gathers its own evidence —
inspecting a git diff, asserting a file is present, orchestrating a test suite.
VERIFY judges the checks the plan already named. The record those judgments are
written into has landed — `Verification`, persisted as append-only history —
but the engine that runs the probes is the rest of Phase 10.

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
│                            # steps; all six wired in; SCHEDULE pairs them.
├── docs/
│   ├── PHASE0-FORENSICS.md  # Read-only audit of both upstream repos:
│   │                        # data models, ~30 vs ~80 MCP tools, feature
│   │                        # comparison, integration risks, boundary design.
│   ├── PHASE1-DOMAIN.md     # The model and the boundary.
│   ├── PHASE2-ADAPTERS.md   # The HandoffAdapter over handoff-mcp on stdio.
│   ├── PHASE2-OBSERVATION.md # The git observation layer.
│   ├── PHASE3-MEMORY.md     # The AiMemoryAdapter over ai-memory.
│   ├── PHASE5-STORE.md      # The SQLite store and the invariants it holds.
│   ├── PHASE6-APP.md        # The control loop, and its six steps.
│   └── PHASE7-SCHEDULE.md  # The scheduler that pairs work with agents.
├── Cargo.toml               # Workspace manifest.
├── rust-toolchain.toml      # Pinned: stable-x86_64-pc-windows-gnu.
└── THIRD_PARTY_LICENSES.md  # MIT notices for both substrates (© 2026 Fabio Akita).
```

### `director-domain` — the vocabulary

Every entity Orqyn speaks, with invariants encoded in types rather than in
prose.

| Module | Entities | Invariant it enforces |
|---|---|---|
| `ids` | 16 identity newtypes + `IdGenerator` | Task identity ≠ agent identity. Distinct, sealed, not interchangeable. |
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
| `verification` | `Verification`, `Probe`, `ProbeKind`, `Evidence`, `EvidenceStatus`, `VerificationStatus`, `VerificationId` | The durable record of judging a task. Append-only history — re-judging writes a new row. Only `Passed`/`Failed` decide; `Unverifiable` is the environment, not the work. |

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

Orqyn's control loop (the shape is fixed; the six steps of the shorthand are
built, and the fuller ordering's remaining steps are not):

```
OBSERVE (git/fs/tests + substrate events + agent heartbeats) ✅
   → UNDERSTAND (project state vs last checkpoint; STATE_CHANGED?)
      → PLAN / REPLAN (grounded in observed state, never invented facts) ✅
         → SCHEDULE (which ready task goes to which agent) ✅
            → ASSIGN (claim lease via HandoffAdapter) ✅
               → MONITOR (heartbeat TTL, lease expiry, progress reports) ✅
                  → VERIFY (runs the plan's named checks itself) ✅
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
- A scheduler round hands every ready task to a distinct available agent, and
  the same plan and registry always yield the same pairings: eligibility, then
  a preference for an agent who has never held the task, then the most
  specialized eligible agent, then an id tie-break. When two ready tasks both
  want the same agent, the earlier one in the plan's order gets it and the
  other is reported rather than silently dropped. Work no available agent can
  do is reported rather than left `todo` with no explanation, and an agent that
  went quiet between the plan and the round is not handed work it cannot
  answer for.
- No crate outside `director-adapters` references a substrate.
- Every source file under `src/` is reachable from the module tree — an
  undeclared `.rs` file is invisible to `cargo`, so a file written but never
  wired in compiles nowhere and its tests never run. The check is a test for
  the same reason the substrate boundary is.
- A task reaching `done` through the verification path always has a verification
  behind it: the verdict, the status move, and the transition recording it are
  one transaction, so the record and the status move together. A verdict that
  cannot attach to a task writes nothing — the foreign key makes the
  judgment-and-move pair all-or-nothing, and the task is left exactly as it
  was.
- A judgment accumulates rather than being overwritten: a task that failed
  verification and passed on the rework carries both rows, newest first, with
  the failed evidence still readable — and an unverifiable round records itself
  while moving the task nowhere, which is what makes resurveying it safe.

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
- **Scheduling, but not load balancing.** The SCHEDULE step is landed: it picks
  which ready task goes to which available agent — eligibility, then a
  preference for an agent who has never held the task, then the most
  specialized eligible agent, then a deterministic id tie-break — at most one
  task per agent per round, and it applies each pairing through ASSIGN, which
  still owns the handoff's legality. What it does not do is balance *load*: it
  does not look at how much work an agent has done lately, where its machine
  is, or a task's estimated effort. A task's `Priority` and `Complexity` exist
  in the model and the scheduler does not read them, because the plan's order
  is already the priority order and a second axis would disagree with it.
  Load- and locality-aware matching is a later phase.
- **No verification engine, only its record layer.** The loop's VERIFY step
  holds the invariant — nothing self-reports completion, and a task reaches
  `done` only because a check Orqyn ran itself passed — but the checks it runs
  are the machine-checkable forms a plan already named. What has landed is the
  *record* those judgments are written into: `Verification` is modelled and
  persisted as append-only history, so a verdict and its evidence trail survive
  the process that made them. What is *not* here is the engine that *gathers*
  its own evidence — inspecting a git diff, asserting a file is present,
  orchestrating a test suite — and writes those records itself. That is the
  rest of Phase 10.
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
| **6** ✅ | `director-app` — the control loop. All six steps landed and tested: OBSERVE (git layer composed with the store), PLAN (validation, ordering, and activation in one transaction, plus the dependency-graph validation it builds on in `director-domain`), ASSIGN (a task handed to an agent and started in one transaction), MONITOR (a lease reclaimed from an agent whose heartbeat went quiet, the session recorded when the agent acknowledged the work, a completion report ending the tenure and moving the task to `verification_pending`), VERIFY (a task judged `done` or `failed` by checks Orqyn runs itself), REPLAN (the states the loop's own verdicts left behind surveyed, and a caller's retry / rework / cancel decision applied to each — a refused round writes nothing, and a cancellation reports the dependents it strands). |
| **7** ✅ | `director-app` — the scheduler. SCHEDULE decides which ready task goes to which available agent, so a loop tick no longer needs somebody to name every assignment: eligibility (can accept work, not already showing a current task, every required capability), then fresh eyes (an agent who has never held the task outranks one who released it, with the prior holder as a fallback rather than a prohibition), then specialist-first (fewest declared capabilities, so generalists stay free for the tasks only they can cover), then a deterministic id tie-break. One task per agent per round. Pairings are computed from a snapshot but applied through ASSIGN, which re-validates against live state, so a stale proposal is refused rather than applied illegally; work no available agent can do is reported rather than silently left `todo`, which is the staffing signal that makes the step worth having over calling `assign` by hand. |
| **10** 🚧 | The verification engine. **Record layer landed:** `Verification` / `Probe` / `Evidence` in `director-domain`, migration `0003_verifications.sql`, `SqliteVerificationRepository`, and `Store::apply_verification` — the verification row, the task's status move, and the transition history in one transaction, with append-only history that survives re-judgment. 23 tests. **Not yet:** the engine that runs the probes — no `director-app` step gathers evidence, so Phase 6's check-based `verify` still holds the invariant. |

---

## Attribution

Orqyn communicates with
[handoff-mcp](https://github.com/alphaelements/handoff-mcp) and
[ai-memory](https://github.com/akitaonrails/ai-memory) as separate works over
MCP. No source is copied from either. Both are MIT (© 2026 Fabio Akita); their
notices are reproduced in [`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md).

## License

MIT — see [`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md).
