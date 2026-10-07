# Phase 10 — The Verification Engine

> **Status: complete.** The engine that gathers evidence and lands a verdict is
> landed and tested in `director-app::engine`. Phase 6's VERIFY step is now a
> report on it rather than the judgment itself, and every task that reaches
> `done` does so because the engine ran a check and recorded what it saw.

## Goal

Phase 6's VERIFY step held the model's central invariant on its own: nothing
self-reports completion, and a task reaches `done` only because a check Orqyn
ran itself passed. That was correct about the judgment and silent about the
reasoning. A task that reached `done` through it carried no durable record of
which checks ran, what they saw, or why the verdict was what it was — the
judgment was as ephemeral as the round that made it.

The record layer closed half of that: `Verification`, `Probe`, and `Evidence`
in `director-domain`, a migration, a repository, and `Store::apply_verification`
that lands a verdict and its consequences in one transaction. What had no
caller was the *engine* — the code that resolves a task's checkable
requirements into probes, gathers the evidence, applies the precedence rules,
builds the record, and writes it. Phase 10 is that engine.

```text
expected outputs ──▶ engine ──▶ probes ──▶ executor ──▶ evidence
                       │                                  │
                       ▼                                  ▼
                  judge(evidence) ──▶ Verification ──▶ Store::apply_verification
                       │                                  │
                       ▼                                  ▼
                 VERIFY's report                task status + history
```

The engine is the only writer of a verdict. VERIFY reads what it produced and
reports it; the engine owns the judgment and the write.

## What the engine is, and is not

**It is independent of the agent that did the work.** `verify_task` takes no
agent id, reads no agent report, and consults no session. It reads what the
executor produces and what the store's own recorded project state says, and
only those. The agent being judged is never asked whether its own work is
finished, and the provider running a command never interprets the command for
the caller. An agent's report is what puts a task in front of the engine; it is
never the verdict the engine reaches.

**It is not a second verification system.** Phase 6's VERIFY ran command
checks; the engine runs the same commands, through the same
`ExecutionProvider`, and reports them as `Evidence`. The `Probe` model exists
so the other questions Orqyn can ask have a place to land when they are
implemented. This phase implements `Probe::Command`, because that is the
question the loop already asks; the other three kinds are reached through
`run_probe`, the single extension point.

**It does not move the task itself.** `verify_task` never calls `update_task`.
The verdict and the status move are one transaction inside
`Store::apply_verification`, which is what makes "a `done` task always has a
verification behind it" a property of the store rather than a habit of the
caller.

## One round of the engine

`engine::verify_task(store, executor, task, working_dir, now)`:

1. **Resolves probes** — `probes_for` walks the task's expected outputs and
   turns each checkable one into a `Probe::Command`. An expected output with no
   check, or a blank one, yields no probe: it is prose, and the engine does not
   invent a question for it.
2. **Gathers evidence** — each probe goes through `run_probe` and the executor.
   A command that runs is `Passed` or `Failed` on its exit code, with both
   streams kept, because that is what a human needs to see why work failed. A
   timeout is `Failed`: a hung suite is an observed outcome, not missing
   evidence. A command the executor refuses, or one that names no program, is
   `Unverifiable` — the environment, not the work.
3. **Reaches a verdict** — `judge`, the pure half, applies the precedence rules
   below.
4. **Pins the state** — `observed_state` reads the project state Orqyn already
   recorded, for the repository and `HEAD` commit the evidence was gathered
   against. The source is the store rather than a fresh git observation on
   purpose: the engine judges against the state Orqyn believes the project is
   in, not whatever git holds mid-round.
5. **Lands it** — `Store::apply_verification` writes the verification row,
   derives the task's destination from the verdict, and writes the transition
   history row, in one transaction.

The evidence is derived once — the verdict is reached on it and the record
persists it, and deriving it twice would be two chances to disagree.

## The precedence rules

`judge` is pure — no store, no I/O, no executor — so the whole policy is
testable without running anything. The order of precedence *is* the design:

1. **`Failed` decides first.** One observed failure outweighs any amount of
   passing or missing evidence. A task with one failing check is `Failed` no
   matter what else the round saw.
2. **`Unverifiable` is second.** Evidence the round could not gather means the
   verdict is not reachable — but only when nothing failed, because a task with
   one failing check and one unrunnable one *has* been judged. This is the
   asymmetry that keeps a broken harness from marking good work failed.
3. **`Passed` is third.** At least one decisive pass is required; a task whose
   criteria are all prose has nothing Orqyn verified, and Orqyn does not
   complete work it did not check.
4. **`Observed` never decides.** Advisory evidence is reported in the record
   and then deliberately ignored — a diff is circumstantial, so it can inform a
   human without satisfying a criterion.

## The version guard, and what a stale round costs

The engine hands the store the `state_version` of the task it read *before* it
gathered its evidence. A verdict is a statement about the work the task held at
a specific version; if the task moved between the read and the landing, the
evidence no longer describes the work, and the update matches no row — the
whole transaction becomes a `StateVersionConflict`, including the verification
row, so a stale round cannot land a judgment against a task somebody else
already moved.

This is the same guard `update_task`, `cancel_task`, and `update_agent` carry,
and for the same reason: two writers who both read version *n* cannot both
write *n+1*. A verdict that moves nothing — an `Unverifiable` round — does not
touch the task row, so it cannot conflict. Its evidence trail lands regardless,
which is what makes "Orqyn looked, and could not yet tell" history worth
keeping.

## What VERIFY is now

The step's surface did not change shape: it still returns a `VerifyReport` of
`Judged` tasks with per-check `CheckResult`s, and its three verdicts are the
same. What changed is who decides.

- `verify` surveys the tasks awaiting a verdict and calls the engine for each.
  It no longer calls `update_task` at all; the `Judged` entries it returns are
  a read of what the engine and the store decided.
- The report keeps the detail the record deliberately drops. `ProbeOutcome`
  carries the command's stdout and stderr and exit code — too rich and too
  report-shaped to be a stored column — and `CheckResult` surfaces it, so a
  caller shows a human the same failure the record summarizes.
- `checks_for` walks the task's whole contract, so a criterion the planner
  wrote as prose is reported as `Unchecked` rather than quietly satisfied,
  in the task's own order, interleaved with the checks that ran.
- The step's `judge` delegates to the engine's, over its own report types, so
  the precedence rules live in one place and the two shapes cannot disagree.

The unverifiable distinction is re-derived for the report from the evidence the
round gathered, because the model keeps `Unverifiable` as one status — the
reason moves nothing, so it is for the caller, not the task. An empty evidence
list is `NoChecks`; a list holding an unrunnable check is `EvidenceMissing`. A
caller fixes different things for each.

## What the tests prove

`judge` and `probes_for` are unit-tested without a store, because the
precedence rules and the probe resolution are the parts most likely to be
subtly wrong and the cheapest to pin down.

`tests/engine.rs` drives the engine against a real store and a real
`LocalExecutor`, and its tasks arrive at `verification_pending` the way the
running loop produces them — ASSIGN hands them out, then MONITOR's
`report_done` records the holder finishing — rather than a status being poked
by hand. A mocked executor could confirm that the engine calls `run_command`,
but not that an exit code the executor produced and a verdict the engine
reached agree.

- A passing command probe lands a passing verification, a failing one lands a
  failing one, and a command the environment cannot run is `Unverifiable`, not
  `Failed`.
- `Unverifiable` evidence does not mark the task failed, and an unverifiable
  round records itself and moves nothing — the task is left exactly as the
  round found it, safe to survey again next tick.
- The verification carries the evidence and the pinned repository state, and
  re-judging a task accumulates history instead of overwriting it.
- **An agent's self-report cannot complete a task**: after `report_done` the
  task is still `verification_pending` and no verification exists for it.
- **The engine never writes the task row itself**, and the witness to that is
  atomicity — the verification row, the status move, and the history row carry
  one timestamp, and the task's version advanced exactly once.
- **A stale engine round cannot move a task that already moved**: two rounds
  read the same version, the loser is refused with a `StateVersionConflict`,
  and it leaves no judgment behind — history holds one entry, not two.
- A task reaching `done` through the step has a verification record, and the
  done task and its verification agree about the evidence.

`tests/end_to_end.rs` runs the whole loop — PLAN, ASSIGN, `report_done`,
VERIFY — and is the test that answers the acceptance criterion directly: a
task the loop completes carries a durable verification, and a failed
verification is recorded as `failed` for REPLAN to act on.

## What is deliberately not here

Only `Probe::Command` is implemented. `TestSuite`, `Diff`, and `File` are
modelled in `director-domain` and answered as `Unverifiable` by `run_probe`,
with the record saying why. That is the honest answer — "Orqyn looked and could
not yet tell" — and it is what keeps an unimplemented probe from being mistaken
for a passed or a failed one. Adding a probe kind later is adding an arm to
`run_probe`, and nothing else.

The engine also does not interpret the evidence beyond the precedence rules. A
diff that looks right, a log line that suggests a partial fix — those are
`Observed`, advisory, and reported to a human without deciding anything.
Judging prose is a later phase's work, and it is not this one's.
