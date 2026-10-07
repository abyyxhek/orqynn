# Phase 11 — Runtime & Recovery Validation

> **Status: complete.** The loop is proven to persist its state across a
> restart, to detect and recover from an agent's disappearance, to replan and
> re-hand failed work to a replacement, and to reach `done` afterwards only
> through independent verification. Five new integration tests cover it; the
> workspace suite is green and both live substrate suites are green.

## Objective

Phases 1–3, 5, 6, 7, and 10 built the architecture. Phase 11 does not add to
it. It proves the property the architecture exists for:

> Orqyn can observe, plan, schedule, assign, monitor, verify, recover from
> failure, replan, and continue work **without losing persistent project
> state.**

The scenario the phase is built around, end to end and without a single status
being poked by hand:

```text
Agent A works on a task
        ↓
Orqyn persists state
        ↓
Agent A disappears
        ↓
MONITOR detects the failure
        ↓
the task returns to a handable state
        ↓
a replacement agent is selected
        ↓
the replacement reconstructs the context Orqyn already holds
        ↓
the replacement continues the task
        ↓
VERIFY
        ↓
DONE
```

## The runtime model, as it actually is

Phase 11 read the code rather than trusting the earlier audits, and three facts
about the runtime shape every conclusion below:

1. **The loop is a library, not a process.** There is no Orqyn binary and no
   MCP server of Orqyn's own. Each step is an `async fn` that takes a `&Store`,
   and the caller — today, the tests — decides when a tick happens. There is no
   scheduler loop, no timer, no daemon.

2. **Every step writes to Orqyn's own SQLite file, and no step makes an MCP
   call.** `OBSERVE` uses the local git2 observation layer; `PLAN`, `SCHEDULE`,
   `ASSIGN`, `MONITOR`, `VERIFY`, and `REPLAN` use the `Store` and, for
   verification only, an `ExecutionProvider`. The MCP adapters
   (`HandoffAdapter`, `AiMemoryAdapter`) implement the provider traits and are
   exercised by the live suites, but **no step of the loop calls them** —
   `director-app` has no reference to either adapter outside its tests. The
   substrates are an integration surface waiting for a driver, not a dependency
   of the loop.

3. **"Restart" means reopening one file.** A `Store` is a connection pool over
   a single SQLite path. Dropping the last clone tears the pool down and closes
   the connections; `Store::open` on the same path re-reads everything, applies
   migrations idempotently, and continues. There is no in-memory state to
   restore because there is no in-memory state — the store *is* the state.

Consequences: because the loop never depends on a remote substrate, restart
recovery is a purely local question with no distributed-consistency hazard. A
task's durability is SQLite's durability.

## Persistence guarantees

Proven by `persistent_state_survives_a_process_restart_and_the_loop_continues`,
which drives four tasks through the loop to a state worth restarting from (work
done, work failed, work cancelled with a recorded decision, work still
`todo`), closes the store, reopens the same file, and continues the remaining
work — with every entity read back rather than remembered.

| Entity | Survives restart? | Evidence in the test |
|---|---|---|
| Task | yes, identity and all fields | `reload` after the restart reads the objective and the acceptance contract the plan named |
| Task status | yes | `Done` / `Done` / `Cancelled` / `Todo` all read back |
| `state_version` | yes, and continues rather than resetting | the version read after the restart equals the one read before it |
| Assignment (active) | yes | the crash-equivalent test finds an `in_progress` task still backed by its active assignment |
| Assignment history | yes, chronological | AUTH's tenure is still `Released` with reason `WorkComplete` |
| Plan | yes, and still the *active* one | `active_plan_for_project` returns `PLAN-1` with all four tasks |
| Decisions | yes, attached to their task | the cancellation's rationale reads back through `decisions_for_task` |
| Transition history | yes, in order | AUTH's trail still reads `todo → in_progress → verification_pending → done` |
| Verification records | yes, with their evidence | one `Passed` for AUTH, one `Failed` for EXTRA, each carrying the evidence the engine gathered |
| Agents | yes | `restart` asserts both agents come back out of the store |
| Checkpoints | yes | the context-continuity test reads one back (written through the repository; see limitations) |

Two details in that list are the point rather than incidental. The
verification id for the task finished *after* the restart continues the
sequence the store holds (`VER-TAIL-1`), because `next_verification_id` counts
the judgments already on record in the store rather than a per-process counter
— a re-judgment after a restart is a new row, never a collision. And the
`state_version` does not reset, because it is a column, not a memory counter.

## Failure behavior

Two recovery paths exist, and they are deliberately not the same one. Confusing
them is the easiest way to get this model wrong.

**Path 1 — the holder vanished.** `MONITOR` derives liveness from heartbeat age
(< 30 min fresh, < 60 min stale, ≥ 60 min gone) and, for a task it finds gone,
calls `Store::expire_lease`. That is one transaction that releases the tenure
as `LeaseExpired`, closes a live session as `Vanished`, marks the agent
`Disconnected`, clears its `current_task`, and puts the task back to `todo`
with a transition row. The work is unfinished, not failed — an expired lease is
not a verdict on anyone's work.

**Path 2 — the work was judged.** `VERIFY`'s engine reaches `Failed`, or a task
is found `in_progress` with no tenant (`Orphaned`), or a dependency reached a
dead end (`PremiseBroken`). Those are the three conditions `REPLAN` surveys, and
only those. Three remediations answer them: `Retry`, `Rework` (amend the
contract — the remediation for a *criterion* that was wrong), and `Cancel`
(terminal, with the reason persisted as a `Decision`).

The distinction the tests pin down: a task whose lease ran out is already back
to `todo`, which is handable, so **`REPLAN` surveys nothing for it** — the
agent-failure test asserts exactly this (`is_settled()` over an empty decision
list). The two steps do not overlap, and neither does the other's job. REPLAN
is for *verdicts and orphans*; MONITOR is for *leases*.

### The failure matrix

| # | Scenario | Persisted state | Task status | Recovery | Data loss? | False completion? |
|---|---|---|---|---|---|---|
| A | Agent disconnects (heartbeat ≥ 60 min) | tenure `Released`/`LeaseExpired`, agent `Disconnected`, transition row | `todo` | SCHEDULE re-hands it (fresh-eyes prefers an agent who never held it) | no | no |
| B | Agent goes quiet but not gone (30–60 min) | nothing written — `Stale` is a report | `in_progress` | the lease stands; next tick re-checks | no | no |
| C | MCP call fails | n/a — **no loop step makes an MCP call** | unchanged | the adapter returns `TransportError::ToolFailed` to its caller | no | no |
| D | MCP returns malformed data | n/a — same | unchanged | `TransportError::Malformed`; an unknown substrate status maps to `VerificationPending`, never a guess | no | no |
| E | A SQLite transaction fails mid-write | nothing — the transaction rolls back | unchanged | the caller retries; existing tests prove rollback by failing an FK on purpose | no | no |
| F | Verification fails | `Verification` row `Failed` with evidence | `failed` | REPLAN retries, reworks, or cancels | no | no |
| G | Verification is unverifiable | `Verification` row `Unverifiable` | unchanged (`verification_pending`) | re-surveyed next tick; a broken environment is not a verdict | no | no |
| H | Stale `state_version` | nothing — the update matches no row | unchanged | `StateVersionConflict` to the caller | no | no |
| I | Duplicate verification id | nothing — `verifications.id` is the primary key | unchanged | `ConstraintViolation` to the caller | no | no |
| J | Process restart | everything above | as left | reopen the file and continue | no | no |
| K | Replacement agent assignment | a second tenure beside the first | `in_progress` | the replacement reports and the engine judges | no | no |
| L | Replan after failure | the remediation, plus a `Decision` for a cancel | `todo` or `cancelled` | SCHEDULE hands the reworked task out again | no | no |

The two "no" columns are the whole point and they are not luck. "No data loss"
follows from the transactional writes: `assign_and_start`, `expire_lease`,
`report_completion`, `apply_verification`, and `cancel_task` each fold several
writes into one transaction, so a refused or interrupted operation leaves the
state exactly as it found it — proven by the store suite's rollback tests. "No
false completion" follows from the invariant in the next section.

## Verification after recovery

The recovered task goes through the same engine as every other task, and that
is provable rather than assumed.

**The direct-completion audit.** A workspace-wide search for `TaskStatus::Done`,
plus a trace of every `UPDATE tasks SET status` in `director-store`, finds six
write sites. Five of them write a status that is not `Done`; the sixth is the
only path to it:

- `Store::apply_verification` — the destination is *derived from the verdict*
  through `VerificationStatus::task_status`, not taken from a caller's task.
  Reached only by `engine::verify_task`, itself reached only by `verify::verify`.
  This is the **only** path to `Done`.
- `assign_and_start` — hardcoded `InProgress` (ASSIGN and SCHEDULE).
- `expire_lease` — hardcoded `Todo` (MONITOR's lease expiry).
- `report_completion` — hardcoded `VerificationPending` (MONITOR's
  `report_done`; the report stops here by construction).
- `Store::cancel_task` — hardcoded `Cancelled`, together with the `Decision`
  that explains it (REPLAN's cancel).
- `TaskRepository::update_task` — the one site that accepts a caller-supplied
  status, and in non-test code only REPLAN calls it, whose remediations
  resolve to `Todo` (`remediation_destination`: `Retry`/`Rework` → `Todo`,
  `Cancel` → `Cancelled`, and the cancel arm routes through `cancel_task`
  anyway). `verify.rs` does not call it at all.

So `Done` has exactly one writer, and it writes `Done` only when the engine's
own verdict is `Passed`. Every other occurrence of `TaskStatus::Done` in the
workspace is a read (the dependency check in `ASSIGN`, the status comparison in
the handoff mapping), a pure mapping function (`VerificationStatus::task_status`,
`verdict_destination` — both report-only, neither writes), a doc comment, or
test code.

The substrate side enforces the same rule independently: an agent's
self-reported `done` arrives as `VerificationPending` unless the
`director-verified` marker Orqyn itself wrote is present. The live test
`an_agent_done_does_not_complete_but_a_director_done_does` proves it on the real
wire, and it is still green.

Two of the new tests close the loop on the recovery scenario specifically: in
the restart test the task finished *after* the restart reaches `done` only
through a verification the engine ran on the recovered state, and in the
crash-equivalent test the second store's verdict is what completes the work the
abandoned store started. Neither test moves a task to `done` itself.

## Context reconstruction

The loop has no "hand a replacement agent its context" step, and Phase 11 did
not invent one — that would be an unrelated feature. What it has is the store,
and `a_replacement_agent_can_be_given_everything_the_prior_agent_left_behind`
proves the store alone is sufficient to brief a replacement mid-flight. Given a
task id, a driver reads:

- **The contract as it now stands** — the amended objective and the corrected
  acceptance criterion, not the ones the first agent failed against.
- **The current state and version** — so the replacement presents the version
  the store is actually on when it writes back. The test asserts the briefing's
  read version is live by showing the subsequent report advances it.
- **Who held it before, and why they are not holding it now** — the released
  tenure with its reason (`WorkComplete`, `LeaseExpired`, …), chronological.
- **What was already judged, and what the evidence said** — the failed
  verification with its evidence detail, so the replacement does not repeat a
  known failure.
- **Where the prior agent got to** — the checkpoint's progress narrative and
  next action.
- **What changed most recently** — the last transition row.

The test stops mid-flight on purpose (the replacement is assigned but has not
reported) so that everything it asserts is context for work still in progress,
not a retrospective.

## The crash-equivalent test

A true SIGKILL test needs a second process, and there is no process to kill —
the loop is a library. `an_abandoned_process_leaves_committed_state_intact_and_recoverable`
tests the property a crash would expose instead, deterministically and without
an OS-specific harness: the first store is *abandoned*, never closed cleanly,
exactly as a process that vanished would leave it; a second store opens the
same file and must recover every committed fact. The assertion that matters is
the consistency one — the recovered task is `in_progress` *because* an active
assignment is still behind it and the agent's `current_task` still points at
it, which is the state a non-atomic handoff would have split across the crash.
The test then lets the first store go *after* the second has taken over the
file, the messiest possible handoff, and the completion report and verdict
still land.

## The tests

All five live in `crates/director-app/tests/runtime_recovery.rs`, all against a
real store on a real file, and all driving the real steps in the real order.

1. **`persistent_state_survives_a_process_restart_and_the_loop_continues`** —
   four tasks (two passing, one failing and cancelled, one left `todo`), a
   store closed and reopened on the same file, then the remaining task finished
   by the recovered loop. Asserts every entity in the table above.
2. **`an_abandoned_process_leaves_committed_state_intact_and_recoverable`** —
   the crash equivalent, and the atomicity assertion across the interruption.
3. **`an_agent_that_vanishes_loses_its_lease_and_a_replacement_finishes_the_work`** —
   AGENT-1 goes 90 minutes quiet, MONITOR reclaims, the tenure is retained with
   the vanished agent named in it, REPLAN surveys nothing, AGENT-2 takes the
   work to `done`.
4. **`a_failed_task_can_be_reworked_for_a_replacement_agent_who_then_passes`** —
   the REPLAN path: failure, reworked contract, replacement agent, pass, with
   both judgments on record.
5. **`a_replacement_agent_can_be_given_everything_the_prior_agent_left_behind`** —
   context continuity, mid-flight.

No test constructs the rows it then claims to verify. Every state is produced
by the step that produces it in production, and every assertion reloads from
the store a fresh tick — or a fresh process — would read.

## One defect found and fixed

The baseline run failed 8 tests in `director-adapters`, all at the same line,
all with `repository already registered`. The git store's test helper kept its
state in a temp directory keyed by process id and never removed it; 111 such
directories had accumulated, and the next time the OS handed out a reused pid
the suite read its own stale state and failed before its first assertion. It
looked like a store bug and was the test suite reusing its own garbage. Fixed
by giving each test a fresh `tempfile::TempDir`, which is removed on drop. Not
a product defect, but a genuine defect in the suite that would have made the
"workspace remains green" criterion unprovable.

A second defect class was found by re-running the gates rather than trusting
the claim above: the new test file itself had 16 `cargo fmt` drift sites and one
`clippy` `-D warnings` error (`needless_borrows_for_generic_args`), so `fmt
--check` and `clippy` were in fact failing on the tree this document describes
as clean. Both were repaired — `cargo fmt --all` and the one borrow removed —
and the gate table below was re-verified from a fresh run rather than carried
forward. Same lesson as the standing one in the project notes: a gate result
certifies the exact tree it ran on and nothing said about it since.

## Known limitations

Phase 11 is a validation phase, and these are the honest edges of what it
proves. None is a correctness defect; all are places the architecture is
deliberately incomplete.

- **No process boundary, so no true kill test.** The crash-equivalent test is
  the strongest deterministic substitute available. When a driver process
  exists, a real SIGKILL test becomes possible and should replace it.
- **No loop step writes a checkpoint.** `Checkpoint` has a model, a migration,
  and a repository, but nothing in the loop calls `create_checkpoint`. The
  context test writes one through the repository — the seam a future MONITOR
  extension will call — and says so in its comments. A replacement agent's
  resumption today rests on the task, the tenure history, and the verification
  history; the progress narrative has to be written by the caller until that
  step exists.
- **`Blocker` is not persisted at all.** It exists in `director-domain` with a
  kind, a status, and a resolution, but there is no `blockers` table, no
  migration, and no repository. REPLAN reports stranded dependents and writes
  nothing about them, which is safe because `ready_in_plan` re-checks
  dependencies on every read — but a caller cannot ask the store what is
  blocked, only what is not ready.
- **`ProviderSync` is never written.** The entity that would record the last
  substrate sync attempt and its outcome has a repository and no caller,
  because no step performs a sync.
- **`OBSERVE` depends on a concrete adapter type.** It takes a `GitService`
  and returns a `RepositoryStatusView` from `director-adapters` rather than
  coding against the `ProjectStateProvider` trait. This is a layering wrinkle,
  not a substrate leak — git2 is not one of the substrates, and the boundary
  test still enforces the identifier rule — but it is the one step the loop
  does not reach through a trait.
- **MCP failure behavior is tested at the adapter, not in the loop.** The
  error vocabulary is explicit (`SpawnFailed`, `Malformed`, `ToolFailed`,
  `NoContent`) and the live suites exercise the real wire, but since no step
  makes an MCP call, an MCP failure cannot affect orchestration state today.
  The criterion is satisfied by the adapters' own tests and the live suites;
  the wiring that would make it a loop-level concern is future work.

## Gate state

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | clean |
| `cargo build --workspace --all-targets` | clean |
| `cargo test --workspace` | green, 537 passed, 0 failed, 13 ignored |
| Live: ai-memory (`--ignored`) | 8 passed |
| Live: handoff-mcp (`--ignored`) | 5 passed |
| Boundary test | 3 passed |

The 13 ignored are the live suites and the suites that need a live server;
they are the ones run separately above.

## Verdict

**Complete.** Every acceptance criterion is proven by a test that reaches it
through the loop's own steps: persistent state, task identity, assignment
history, decisions, transition history, and verification records all survive a
restart; an agent that vanishes is detected and its work recovered without a
false completion; a replacement can be assigned and briefed; a replan decision
persists; the recovered task still requires independent verification; no
direct agent-to-`done` path exists; the substrate boundary holds; and both the
workspace suite and both live substrate suites are green. The MCP criterion is
satisfied at the adapter layer with the loop-wiring gap documented above.
