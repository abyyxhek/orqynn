# Phase 1 — Canonical Domain Model and Provider Boundary

> Director Brain · Phase 1 deliverable
> Date: 2026-09-24
> Scope: `director-domain` and `director-adapters` only. No MCP server, no
> storage, no loop, no substrate wiring.

## Goal

Establish Director's canonical domain model and the provider trait boundary,
with **zero coupling to either substrate**, and prove it with tests.

Phase 0 established the rule — Director talks to handoff-mcp and ai-memory
*exclusively over MCP*, never by importing their internal structs. Phase 1 is
that rule made executable: the boundary is now a set of traits, and a test
enforces that nothing outside the adapters crate can even name a substrate.

## What was built

### `director-domain` — the vocabulary

Every entity Director speaks, with invariants encoded in types rather than in
prose.

| Module | Entities | Invariant it enforces |
|---|---|---|
| `ids` | `TaskId`, `AgentId`, `SessionId`, … 13 newtypes + `IdGenerator` | **Task identity ≠ agent identity.** Distinct newtypes; neither constructible from the other. Sealed so the set of identity kinds is closed. |
| `task` | `Task`, `Subtask`, `TaskStatus`, `Priority`, `Complexity`, `ExpectedOutput` | **A task has no agent field.** `Done` is not reachable by agent report (`reachable_by_agent_report`). Readiness is a pure function over dependency statuses; a `Failed` dependency blocks. |
| `agent` | `Agent`, `Machine`, `AgentStatus`, `Harness` | Agents are replaceable, heartbeated, and thin. Liveness windows mirror the substrate's 30/60-minute convention. |
| `assignment` | `AgentAssignment`, `AssignmentStatus`, `ReleaseReason` | Reassignment is a new record, so tenure history accumulates instead of being overwritten. |
| `capability` | `Capability` | Assignment filter, not a skills ontology. `Coding` is a generalist super-capability; only a `Human` satisfies a `Human` requirement. |
| `session` | `AgentSession`, `SessionStatus`, `SessionEnd` | Sessions are evidence trails, not the work. `ended_uncleanly()` is the signal recovery keys on; context exhaustion is *not* unclean. |
| `state` | `ProjectState`, `StateComparison`, `TestResults`, … | Observed, never remembered. `StateComparison` is the resume guard. |
| `checkpoint` | `Checkpoint`, `ContinuationPackage`, `ResumeStatus` | Neither substrate has this. Compares recorded vs observed state, including the rebase case (`CommitGone`). |
| `context` | `RecentContext`, `ContextSnapshot` | **The window is bounded at all times.** Compaction is lossy-but-accountable via `total_recorded`; errors and checkpoints are promoted, not dropped. |
| `action` | `Action`, `ActionKind` | Actions are observed, not self-reported. Importance ranking drives compaction. |
| `plan` | `Plan`, `PlanStatus` | **Plans supersede, they are not edited.** The old plan is retained as audit trail. |
| `decision` | `Decision`, `DecisionStatus` | Decisions carry rationale and rejected alternatives, and can be reversed by a later decision. |
| `blocker` | `Blocker`, `BlockerKind`, `BlockerStatus` | Blocked tasks are observable, so "still blocked after three replans" is a reportable condition. `Obsolete` ≠ `Resolved`. |
| `handoff` | `Handoff`, `HandoffState`, `HandoffError` | **Claim-once**: only the addressed agent may accept, only while open. Task-scoped, unlike either substrate's session-scoped notion. |
| `project` | `Project`, `DefaultBranch` | A root, not a container — tasks are referenced by id, never nested. |
| `providers` | 7 provider traits + `Provider`, `ProviderError`, `Memory`, `MemoryQuery`, `CommandSpec`, `CommandOutcome` | The trait boundary; see below. |

### `providers` — the boundary

Seven traits, all `async`, all returning Director's own types, each with an
associated `Error`:

- `TaskProvider` — CRUD, status, `ready_tasks`, dependency edges.
- `AgentProvider` — registry, heartbeat, availability.
- `SessionProvider` — lifecycle, per-task history, fork.
- `HandoffProvider` — Director's claim-once transfer.
- `MemoryProvider` — durable knowledge, query, recency.
- `ProjectStateProvider` — observed git/filesystem state.
- `ExecutionProvider` — run real commands; exit codes Director reads itself.

Four rules are baked in:

1. Methods are `async` — every real substrate is an I/O boundary.
2. Inputs and outputs are Director's types, never a substrate's struct.
3. Each trait has an associated `Error`, so a substrate's failure vocabulary
   cannot become Director's.
4. **Nothing here completes a task.** There is no method an agent's report can
   reach that sets `Done`. Verification is a Director-owned engine (Phase 10),
   not a provider capability.

Deliberately absent: checkpoint, plan, decision, blocker, verification, and
assignment storage. Those are Director-owned entities in Director's own store
(Phase 5). Exposing them as provider traits would invite a substrate to become
authoritative over Director's own state.

### `director-adapters` — the proof

- `InMemoryProvider` — one struct, every trait, backed by `HashMap`s. It is
  faithful to the trait *contracts* (claim-once handoffs, unique ids,
  dependency-aware readiness) so a test passing against it is evidence about
  the loop, not a tautology.
- `LocalExecutor` — a real `ExecutionProvider` running commands via
  `std::process` with a timeout. This is the fallback the verification engine
  will use when a substrate cannot execute commands.

The real substrate adapters — `HandoffAdapter`, `AiMemoryAdapter` — are Phase 2
and Phase 3 and are **deliberately absent**. Director's core must be provably
independent before any substrate is wired in.

## Definition of done — status

- [x] `director-brain` Rust workspace, edition 2021, toolchain pinned
      (`rust-toolchain.toml`, `stable-x86_64-pc-windows-gnu`).
- [x] All Phase 1 entities present, identity newtypes sealed and distinct.
- [x] Provider traits in `director-domain::providers`.
- [x] `InMemoryProvider` implements every trait.
- [x] Tests: task identity stable across three assigned agents; reassignment
      preserves task fields; assignment history retained.
- [x] `THIRD_PARTY_LICENSES.md` with both MIT notices (© 2026 Fabio Akita).
- [x] Boundary test enforcing no substrate reference outside `director-adapters`.
- [x] No MCP server, no storage, no loop.

## Boundary enforcement

`crates/director-adapters/tests/boundary.rs` walks the workspace and fails the
build if any `.rs` file outside the adapters crate contains the identifiers
`handoff_mcp` or `ai_memory`, or any `Cargo.toml` outside the adapters crate
declares a dependency whose name contains `handoff` or `ai-memory`.

Note the deliberate choice of what counts as coupling. `director-domain`'s doc
comments name both substrates, because explaining *why* Director's model differs
from `TaskData` is the design rationale for avoiding it. That is the opposite of
coupling. The test therefore looks for the identifiers a `use` statement would
need — not for English.

## Environment notes

Two machine-specific issues were hit and worked around; neither is an
architectural decision.

1. **No Rust toolchain** at Phase 0's close. Installed via `rustup`; the
   workspace pins the GNU ABI to match the upstream repos' convention.
2. **No C toolchain** on the machine, so neither `x86_64-pc-windows-gnu` (needs
   `dlltool`) nor `x86_64-pc-windows-msvc` (needs `link.exe`) could link test
   binaries. Resolved with a portable WinLibs GCC/binutils on `PATH`.
3. **The project directory is inside OneDrive.** Its sync engine takes write
   locks on `target/` and breaks builds intermittently. Build with the target
   directory redirected off OneDrive:

   ```sh
   export CARGO_TARGET_DIR="$HOME/.director-brain-target"
   ```

   This is a developer-machine workaround, not a committed repo setting — a
   repo-level `target-dir` would be machine-specific.

## What Phase 1 deliberately does not do

- No persistence. `InMemoryProvider` is in-process only; the store crate comes
  with checkpoints in Phase 5.
- No MCP server. Director's own tool surface is a later phase.
- No planner, no verification engine, no loop. The traits exist; the behavior
  does not.
- No substrate adapters. Until Phase 2/3, Director has no way to reach either
  substrate — by design.

## Test coverage notes

Unit tests are co-located with each entity and exercise the invariants above:
`TaskStatus::Done` is unreachable by agent report; a failed dependency blocks
readiness; the recent-context window never exceeds its bound; a handoff cannot
be accepted twice or by the wrong agent; `StateComparison` distinguishes a head
advance from a rebase. `InMemoryProvider` tests exercise the same contracts
through the async trait boundary, which is what later phases will actually use.
