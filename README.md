# Director Brain

A vendor-independent orchestration brain for multi-agent software work.

Director Brain sits **above** the coding agents (Claude Code, Codex, DeepSeek,
OpenCode, Goose, Cursor, …) and **beside** the memory/tooling substrates
([handoff-mcp](https://github.com/alphaelements/handoff-mcp) and
[ai-memory](https://github.com/akitaonrails/ai-memory)). It owns the parts
neither of those systems has: planning, assignment, checkpointing, independent
verification, recovery, and replanning.

Director talks to both substrates **exclusively over MCP**. It never imports
their internal structs, never forks their source, and never depends on their
crates. A boundary test in this repo enforces that — see
[The boundary is a test, not a convention](#the-boundary-is-a-test-not-a-convention).

> **Status: Phase 1 complete, Phase 2 in progress.** The canonical domain model
> and the provider trait boundary are in place, with zero substrate coupling and
> a passing test suite (174 tests). Phase 2 has two independent halves, and both
> halves have now landed in code:
>
> - **The observation half** — a git2-backed observer, a file-backed store, and
>   the service that composes them into `ProjectStateSnapshot` with
>   deterministic change detection. Read-only by construction: Director never
>   mutates a repository it is verifying.
> - **The handoff-mcp half** — wire types, a stdio JSON-RPC transport, and the
>   bidirectional mapping.
>
> The remaining Phase 2 step is the `HandoffAdapter` struct that composes those
> three into live `TaskProvider`, `AgentProvider`, and `SessionProvider`
> implementations. There is no MCP server of Director's own, no persistence, and
> no loop yet — those are later phases, and their absence here is deliberate.

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
provider method an agent can call that completes a task. Only Director's own
verification engine — which inspects git, files, and test exit codes itself —
can do that.

This is the guardrail behind the acceptance criterion: *"Agent claims 'Done.'
Tests fail → must not become COMPLETED."* Both substrates were audited and
neither satisfies it: handoff-mcp's `handoff_check_criterion` is a self-reported
checkbox tick, and ai-memory has no task model at all.

---

## Repository layout

```
director-brain/
├── crates/
│   ├── director-domain/     # The vocabulary: every entity, identity, status
│   │                        # enum, and provider trait. Knows no substrate.
│   └── director-adapters/   # InMemoryProvider (all traits, in-process),
│                            # LocalExecutor (real command execution), and
│                            # the git observation layer. The ONLY crate
│                            # allowed to name a substrate.
├── docs/
│   ├── PHASE0-FORENSICS.md  # Read-only audit of both upstream repos:
│   │                        # data models, ~30 vs ~80 MCP tools, feature
│   │                        # comparison, integration risks, boundary design.
│   └── PHASE1-DOMAIN.md     # This deliverable: the model and the boundary.
├── Cargo.toml               # Workspace manifest.
├── rust-toolchain.toml      # Pinned: stable-x86_64-pc-windows-gnu.
└── THIRD_PARTY_LICENSES.md  # MIT notices for both substrates (© 2026 Fabio Akita).
```

### `director-domain` — the vocabulary

Every entity Director speaks, with invariants encoded in types rather than in
prose.

| Module | Entities | Invariant it enforces |
|---|---|---|
| `ids` | 13 identity newtypes + `IdGenerator` | Task identity ≠ agent identity. Distinct, sealed, not interchangeable. |
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
| `handoff` | `Handoff`, `HandoffState`, `HandoffError` | Claim-once: only the addressed agent may accept, and only while open. |
| `project` | `Project`, `DefaultBranch` | A root, not a container — tasks are referenced by id, never nested. |
| `repository` | `Repository`, `ProjectStateSnapshot`, `WorktreeState`, `ObservationEvent`, `SyncResult` | What Director learned by looking at git, and how it tells one observation from the next. Neither substrate has this. |
| `providers` | 7 provider traits + `Provider`, `ProviderError`, `Memory`, `MemoryQuery`, `CommandSpec`, `CommandOutcome` | The trait boundary. |

### `providers` — the boundary

Seven traits, all `async`, all returning Director's own types, each with an
associated `Error`:

| Trait | Purpose |
|---|---|
| `TaskProvider` | CRUD, status, `ready_tasks`, dependency edges |
| `AgentProvider` | Registry, heartbeat, availability |
| `SessionProvider` | Lifecycle, per-task history, fork |
| `HandoffProvider` | Director's claim-once transfer |
| `MemoryProvider` | Durable knowledge, query, recency |
| `ProjectStateProvider` | Observed git/filesystem state |
| `ExecutionProvider` | Run real commands; Director reads the exit code itself |

Four rules are baked in:

1. Methods are `async` — every real substrate is an I/O boundary.
2. Inputs and outputs are Director's types, never a substrate's struct.
3. Each trait has an associated `Error`, so a substrate's failure vocabulary
   cannot become Director's.
4. **Nothing here completes a task.**

Deliberately absent from the traits: checkpoint, plan, decision, blocker,
verification, and assignment storage. Those are Director-owned entities in
Director's own store. Exposing them as provider traits would invite a substrate
to become authoritative over Director's own state.

### `director-adapters` — the proof

- **`InMemoryProvider`** — one struct implementing *every* provider trait
  against in-process `HashMap`s. It is faithful to the trait *contracts*
  (claim-once handoffs, unique ids, dependency-aware readiness) so a test
  passing against it is evidence about the loop, not a tautology. This is why
  Director's loop can be developed and tested with zero external processes.
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

The real substrate adapters are **partly built**. `crates/director-adapters/src/handoff/`
holds the handoff-mcp adapter's three layers:

- **`wire`** — serde mirrors of handoff-mcp's JSON shapes. Deliberately not the
  domain types and unable to become them, so every schema difference lives in
  one place.
- **`transport`** — a stdio JSON-RPC client. Director spawns the server as a
  child process and speaks line-delimited JSON-RPC 2.0 over stdin/stdout. It
  never links the substrate's code; it crosses a process boundary.
- **`mapping`** — the bidirectional translation, and the layer that earns its
  keep. The two models are not isomorphic: task statuses (6 vs 8 states),
  priorities, and — critically — the meaning of `done`. handoff-mcp's `done` is
  an agent self-report, so it becomes `VerificationPending`, not `Done`, unless
  Director itself produced it (a marker stashed in the substrate's `extra`
  map). That is the acceptance criterion "agent claims done, tests fail → must
  not become COMPLETED", enforced at the boundary.

The `HandoffAdapter` struct that composes these into `TaskProvider`,
`AgentProvider`, and `SessionProvider` is the remaining Phase 2 step. The
`AiMemoryAdapter` is Phase 3. Until they land, Director's core must remain
provably independent of the substrates — `InMemoryProvider` is the proof.

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
explaining *why* Director's model differs from `TaskData` is the design
rationale for avoiding it. That is the opposite of coupling.

---

## Architecture

```
                    ┌─────────────────────────────────────────────┐
                    │            DIRECTOR BRAIN                    │
                    │  owns: plan, task, assign, verify,          │
                    │   checkpoint, recover, replan, context       │
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

Director's eventual control loop (the shape is fixed now; the behavior is not
built yet):

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

Boundary rules, in full, are in [`docs/PHASE0-FORENSICS.md`](docs/PHASE0-FORENSICS.md)
§6. The short version: Director owns the task lifecycle and everything
upstream of it; the substrates are read/write backends for tasks, sessions,
agents, and memory. handoff-mcp becomes the task/session/agent substrate;
ai-memory becomes the memory/retrieval substrate. Director's checkpoint and
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
> export CARGO_TARGET_DIR="$HOME/.director-brain-target"
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
- `StateComparison` distinguishes a head advance from a rebase.
- Every public entity round-trips through serde — because everything Director
  persists crosses a serialization boundary sooner or later.
- No crate outside `director-adapters` references a substrate.

---

## What Director deliberately does not do yet

- **No persistence.** `InMemoryProvider` is in-process only; the store crate
  comes with checkpoints in a later phase. (The git observation layer's JSON
  store holds observed repository state only — it is a cache for change
  detection, not Director's own state.)
- **No MCP server.** Director's own tool surface is a later phase.
- **No planner, no verification engine, no loop.** The traits exist; the
  behavior does not.
- **No live substrate connection.** The handoff-mcp wire types, transport, and
  mapping are in place, but nothing composes them into a provider yet —
  `HandoffAdapter` is the remaining Phase 2 step. Until it lands, Director's
  core must remain provably independent of the substrates, and
  `InMemoryProvider` is the proof.

## Roadmap

| Phase | Content |
|---|---|
| **0** ✅ | Repository forensics: read-only audit of both substrates. |
| **1** ✅ | Canonical domain model + provider trait boundary, zero substrate coupling. |
| 2 🚧 | **Git observation layer ✅** (observer, store, service, `ProjectStateSnapshot`) **+ handoff-mcp client** (wire, transport, mapping ✅; `HandoffAdapter` next). |
| 3 | `AiMemoryAdapter` — MCP client for ai-memory. |
| 5 | `director-store` — Director's own SQLite: checkpoints, plans, verifications, decisions, recent context, recovery packages. |
| 10 | The verification engine. |
| — | The loop: OBSERVE → PLAN → ASSIGN → MONITOR → VERIFY → REPLAN. |

---

## Attribution

Director Brain communicates with
[handoff-mcp](https://github.com/alphaelements/handoff-mcp) and
[ai-memory](https://github.com/akitaonrails/ai-memory) as separate works over
MCP. No source is copied from either. Both are MIT (© 2026 Fabio Akita); their
notices are reproduced in [`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md).

## License

MIT — see [`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md).
