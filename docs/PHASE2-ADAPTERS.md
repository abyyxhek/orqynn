# Phase 2 — Substrate Adapters and the Git Observation Layer

> Director Brain · Phase 2 deliverable
> Date: 2026-09-25
> Scope: `director-adapters` only. No loop, no persistence of Director-owned
> entities, no MCP server of Director's own.

## Goal

Make Phase 1's trait boundary real against something that is not a `HashMap`.

Phase 1 ended with seven provider traits and one in-memory implementor, which
proves the core is substrate-independent and nothing else. Phase 2 wires
Director to the two things it can actually reach — a real substrate over MCP,
and a real git repository — and proves the boundary holds under that wiring.

The phase has two independent halves, and they are different in kind:

- **The handoff half** is a *substrate adapter*: a client for handoff-mcp over
  stdio JSON-RPC, where every interaction crosses a process boundary and every
  vocabulary mismatch has to be translated deliberately.
- **The git half** is an *observation layer*: git2-backed reading of a working
  tree, plus a file-backed store for what Director has recorded about it. Git is
  a tool Director reads, not a substrate Director negotiates with, so this half
  never crosses the MCP boundary at all.

## The handoff half

Four modules in `crates/director-adapters/src/handoff/`, each with one job:

| Module | Job |
|---|---|
| `wire` | Serde mirrors of handoff-mcp's JSON shapes. Deliberately *not* the domain types and unable to become them, so every schema difference is visible in one place. |
| `transport` | A stdio JSON-RPC 2.0 client. Spawns `handoff-mcp` as a child and speaks line-delimited JSON over stdin/stdout. Director never links the substrate's code. |
| `mapping` | Bidirectional translation between the two models, and the record of every place they are not isomorphic. |
| `adapter` | [`HandoffAdapter`] — `TaskProvider`, `AgentProvider`, and `SessionProvider` made real by composing the three above, plus the state none of them can carry. |

[`HandoffAdapter`]: ../crates/director-adapters/src/handoff/adapter.rs

### What the mapping is not allowed to assume

The two task models are not isomorphic, and three vocabularies do not line up.
Each is handled explicitly rather than coerced:

1. **Status.** The substrate has 6 states; Director has 8. Director-only states
   fall back to the nearest state the substrate can express.
2. **Priority.** The substrate has `low`/`medium`/`high`; Director adds
   `Critical`, which maps down to `high`.
3. **`done`.** The load-bearing one. handoff-mcp's `done` is an agent
   self-report — Phase 0 finding R6: `handoff_check_criterion` is a checkbox an
   agent ticks. A substrate `done` therefore never becomes Director `Done`; it
   becomes `VerificationPending`, because nothing verified it.

That third rule is the acceptance criterion — *agent claims done, tests fail →
the task must not become COMPLETED* — enforced at the boundary instead of in the
loop. It is the reason the adapter exists as a layer rather than as a set of
`From` impls.

### What the `extra` channel turned out to be worth

`TaskData.extra` is a genuine `#[serde(flatten)]` map inside the substrate's
storage layer, so it was natural to plan on stashing Director-only state there:
`Backlog`/`Failed`/`Cancelled`, `Critical`, capabilities, complexity, and the
`director_verified` marker that would let a `done` be trusted.

Auditing the substrate's handlers, then running against the live server, showed
the channel is closed at the MCP boundary in **both** directions:

- **Reads.** `handoff_get_task` builds its reply from named fields; `extra` is
  never serialized onto the wire.
- **Writes.** `handoff_update_task` reconstructs the record from named fields. A
  *create* starts from an empty map; an *update* copies only known fields. In
  neither case does anything Director sends in `extra` reach the file.

So a Director-only status and a `Critical` priority do **not** survive a
substrate round trip. The write path still populates `extra` — it is the right
shape if the substrate ever exposes the field, and it costs nothing — but
nothing depends on it. What the boundary *can* rely on is the **trusted-done
set**: the ids Director itself moved to `Done`, kept by the adapter and handed
to the mapping on every read. Because only Director's own writes can add to it,
an agent cannot promote a task by writing `done`.

This is why the adapter is generic over a [`HandoffWire`]` trait with one method:
the assumption that `extra` round-tripped survived unit tests against a fake and
died only against the real server. Two concrete wire bugs — a `done_criteria`
field named `text` where the substrate sends `item`, and a `TaskLink` shape of
`{kind, url}` where the substrate sends `{target, link_type}` — were invisible
to the in-memory tests for the same reason. Both would have failed to parse real
replies.

[`HandoffWire`]: ../crates/director-adapters/src/handoff/adapter.rs

### Write-path repairs

Two substrate rules would reject Director's writes outright, so the adapter
addresses them at the boundary instead of working around them at call sites:

- **Criteria must be checked to reach `done`.** The substrate bails on an
  unchecked criterion. Director reaches `Done` only through its own
  verification, so the adapter ticks the criteria when it writes a Director
  completion — recording a verdict, not rubber-stamping a claim, because an
  agent's report never reaches that path.
- **Estimates are required.** `require_estimate_hours` defaults on and rejects
  any `in_progress`/`review`/`done` write without `schedule.estimate_hours` —
  which covers almost every status Director writes, since
  `VerificationPending` maps to `in_progress`. Director never writes an
  estimate: inventing hours would corrupt the substrate's metrics. Instead the
  adapter disables the setting in Director's own integration project at setup.

### Identity is process-global

handoff-mcp holds one process-wide agent identity derived from
`CLAUDE_SESSION_ID` at `handoff_load_context` time; there is no per-request agent
id. The transport bakes one identity into the child's environment at spawn.

One consequence is a constraint, not a limitation of this code: **one adapter
instance speaks as one agent**. The adapter leans into it — `register_agent` and
`heartbeat` are refused for any identity but the connection's own, with an
`Unsupported` error, rather than misattributing work to an agent the process
cannot be. The pool that manages several identities is Phase 8.

The ordering of `env`/`env_remove` in `McpTransport::spawn` is load-bearing for
exactly this reason and is not obvious: `Command` applies removals after sets,
so clearing the ambient variable must happen *before* setting Director's, or the
removal also deletes the value Director set. Live testing caught this; the
symptom was the substrate registering agents under a generated timestamp id
instead of Director's.

### Where the substrate has no answer

The adapter says so instead of approximating:

- `set_agent_status` — the substrate derives status from heartbeat age on every
  read, so there is nothing to write. An operator taking an agent offline is a
  Director-owned fact for Director's own store (Phase 5).
- `HandoffProvider`, `MemoryProvider`, `ProjectStateProvider`,
  `ExecutionProvider` — not implemented here. The substrate's handoff is
  session-scoped notes, not Director's claim-once transfer; its memory tools are
  the ai-memory adapter's job (Phase 3); observing git is the git half below;
  running commands is `LocalExecutor`.

## The git half

Three modules in `crates/director-adapters/src/git/`, and the domain types they
produce in `crates/director-domain/src/repository.rs`. The split follows a rule
from Phase 2's design: *observing git, interpreting what it means, and deciding
what to do about it are three different jobs*, and only the first two live here.

| Module | Job |
|---|---|
| `observer` | "What happened in git?" A read-only lens over one working tree, backed by git2. Persists nothing. |
| `store` | "What has Director recorded?" One JSON file per repository, written atomically, with optimistic version checks. |
| `service` | "What does this mean for the project state?" Composes the two, runs the deterministic comparison, persists the result. |

git2 rather than the git CLI, because the specification asks for a proper git
library and warns against fragile shell-string parsing — and using the library
removes an entire class of command injection: there is no shell to inject into,
and a caller supplies a path, never a command.

The trait this half serves, `ProjectStateProvider`, is the most rule-laden of
Phase 1's traits, and the rule is **observed, never remembered**: a substrate may
cache, but the truth is what git and the filesystem say right now. The
verification engine (Phase 10) depends on that being trustworthy.

## Evidence

### Gates

All four are clean as of this writing:

```sh
cargo fmt --check                      # clean
cargo clippy --all-targets -- -D warnings   # clean
cargo test --all                       # 190 tests, 0 failures
cargo build --all                      # clean
```

### Test counts and what each is evidence of

| Tests | About |
|---|---|
| 128 `director-domain` unit | Entity invariants from Phase 1, unchanged. |
| 59 `director-adapters` unit | The adapters crate, of which 15 drive `HandoffAdapter` through a fake `HandoffWire` and 2 cover the corrected session mapping. |
| 3 `tests/boundary.rs` | The substrate-coupling rule: nothing outside `director-adapters` may name a substrate. |
| 5 `tests/handoff_live.rs`, `#[ignore]` | **Live** runs against a real `handoff-mcp` server. |
| 1 doc-test | The id generator example. |

The adapter's 15 unit tests are written against a fake that mirrors what the
substrate *should* do. They are evidence about the adapter's logic — the
verification rule, the write repairs, the identity refusal, the load-modify-store
status change. They are deliberately *not* evidence about the wire, because a
fake cannot disagree with the mirror it was written from. That is the live
suite's job.

The five live tests are ignored by default because they need the built server
binary and write to the filesystem:

```sh
HANDOFF_MCP_BINARY=/path/to/handoff-mcp(.exe) \
  cargo test -p director-adapters --test handoff_live -- --ignored --nocapture
```

They cover the round trip, both branches of the `done` rule, a completion with
criteria the substrate would otherwise reject, the agent and session round trip,
and the identity refusal. All five pass against v0.35.1 as of this writing.

## Definition of done — status

- [x] `HandoffAdapter` implements `TaskProvider`, `AgentProvider`,
      `SessionProvider` over a live handoff-mcp.
- [x] The verification rule holds at the boundary, and is live-tested in both
      directions.
- [x] Wire mirrors verified against the live server, with two field-shape bugs
      found and fixed by that verification.
- [x] The git observation layer: observer, store, service, plus
      `director-domain::repository`.
- [x] All four gates clean.
- [x] The `extra` limitation is documented in code, not just here.

## What Phase 2 deliberately does not do

- **No persistence of Director-owned entities.** Checkpoint, plan, decision,
  blocker, verification, and assignment storage is Phase 5. The adapters expose
  nothing for them, because no substrate owns them.
- **No ai-memory adapter.** That is Phase 3.
- **No multi-agent pool.** One adapter speaks as one agent, by the substrate's
  design.
- **No verification engine.** The boundary makes the rule enforceable; Phase 10
  is where Director decides whether a task is really done.
- **No recovery of Director-only state through the substrate.** Where the
  substrate cannot carry a value, the adapter degrades to the nearest honest
  state and says so, rather than encoding state in a field that belongs to the
  substrate.
