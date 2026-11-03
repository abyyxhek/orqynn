# Multi-Agent Coordination Audit — Orqynn (Phase 42)

**Date:** 2026-11-03
**Audited tree:** `main` — noted below, because the tree moved during the audit
**Scope:** agent registration and identity, eligibility, task assignment, scheduling
policies, task ownership, lease creation and expiration, claim-once behavior, task
versioning and CAS, handoff between agents, verification and completion, recovery
of interrupted tasks, SQLite transaction boundaries.
**Method:** static audit of the implementation plus new concurrency probes written
against isolated, offline SQLite fixtures. No live agents, no gateway or credential
changes, no commits. Everything below is backed by either code quotes with
file:line or an executed test that reproduces it.

> **Note on tree state and a file collision.** When this audit started, `main` was
> at `88fc846` with staged, uncommitted work. During the audit window another
> session committed `54391ab` (the `handoffs` CLI command) and `c3243cf` (docs
> refresh), so HEAD moved; those commits do not touch any coordination code this
> audit examined, and the probes below were re-run green after them. A pre-existing
> `MULTI_AGENT_AUDIT.md` from an earlier phase was already present at this
> filename when this task began; it cites test files that do not exist in this
> tree (`director-store/tests/assignments.rs`, `director-adapters/tests/conflict.rs`,
> `director-app/tests/handoffs.rs`) and concludes no defects. It has been preserved
> untouched as `MULTI_AGENT_AUDIT.phase35-previous.md`; this document, backed by
> executed probes, is the Phase 42 deliverable.

---

## 1. Architecture overview

Orqyn is a *director* that drives external coding agents. It is not a peer-to-peer
agent mesh: one process orchestrates, and the agents are substrates it launches and
monitors. A second Orqyn process on the same database is a legitimate deployment
(restart, horizontal scale), so the invariants have to hold in storage rather than in
the director's memory. The workspace is five crates:

| Crate | Role |
|---|---|
| `director-domain` | The model: `Task`, `Agent`, `AgentAssignment`, `Handoff`, `Checkpoint`, `Verification`, and the repository traits. No I/O. |
| `director-store` | SQLite implementation of those traits. Every coordination invariant that matters lives here. |
| `director-app` | The loop's steps: OBSERVE, PLAN, SCHEDULE (`schedule.rs`), ASSIGN (`assign.rs`), MONITOR (`monitor.rs`), VERIFY (`engine.rs`), checkpoint (`checkpoint.rs`), recovery (`recovery.rs`), REPLAN. |
| `director-adapters` | Git observation, the agent harness drivers, the handoff MCP client, the ai-memory adapter. |
| `orqynn` | The CLI (`run`, `resume`, `handoffs`) and the operator-facing lifecycle. |

**Storage configuration** (`crates/director-store/src/connection.rs`):

- `journal_mode = WAL` (asserted, not assumed — `set_pragmas` fails if the file
  settled for rollback mode, `connection.rs:173-181`).
- `foreign_keys = ON` on every pooled connection (`connection.rs:184-185`), enforced
  by a test that inserts an orphan task and expects refusal.
- `synchronous = NORMAL` — a deliberate, documented trade: survival of a *process*
  restart, not a power pull (`connection.rs:22-27`).
- `busy_timeout = 5000ms`, so lock contention waits rather than erroring
  (`connection.rs:192-193`).
- A pool of 8 connections; `pool.get()` **panics** if exhausted, on the principle
  that a held-across-an-await connection is a bug worth stopping for
  (`connection.rs:126-136`).

Repository calls are synchronous rusqlite work inside `async fn` — they contain no
yielding await. Consequently **`tokio::join!` is not concurrency for this store**:
a joined future runs to completion before the other is ever polled. Every race that
matters has to be driven by real OS threads or a second process. This is documented
in `assign_concurrency.rs:31-47` and honored throughout the existing test suite.

**The id model.** All ids are human-readable TEXT primary keys (`ASG-AUTH-3` is the
third tenure of `AUTH`), allocated by counts taken *inside* the write transaction
(`next_assignment_id`, `assignments.rs:543-559`; `next_verification_id`,
`verifications.rs:511-527`). Nothing depends on a process-local counter, so ids
survive restarts and ordinals never collide across processes.

---

## 2. Agent coordination model

### Registration and identity

Agents register themselves before they can receive work: `Agent::register`
(`director-domain/src/agent.rs:198-222`) with a **caller-chosen id**, persisted by a
plain `INSERT` (`director-store/src/agents.rs:41-68`) whose primary key is the id. A
re-registration of an existing id fails on the PK; `orqynn`'s CLI treats a duplicate
as "already registered, reusing it" (`orqynn/src/lifecycle.rs:323-339`) — meaning a
retried run inherits whatever `status` and `current_task` a prior run or a lease
expiry left behind.

### Assignment is a first-class, append-only link

A task row never names an agent; the `agent_assignments` table does. Ending an
assignment **marks a row `released`, never deletes it** ("the task outlives the
agent", `assignments.rs:4-7`), so "who worked on this task, and why did each one
stop" is an ordinary query. The `TaskAlreadyAssigned`-style refusal belongs to the
app layer only (§6, D3).

### Lease and liveness

Leases are not stored timers. Liveness is **derived on every read** from
`agents.last_seen` (`director-domain/src/agent.rs:253-270`): silence under 30 minutes
is `Busy`, 30–60 minutes is `Stale` (a report, not an intervention), past 60 minutes
is `Gone` and MONITOR expires the lease. Expiry is one transaction
(`assignments.rs:886-1033`): release the tenure as `LeaseExpired`, close a still-live
session as `Vanished`, clear the agent's `current_task` view, set the agent
`Disconnected` (unless an operator's `Offline` stands), put the task back to `todo`.

### Scope as ground ownership

Only `in_progress` work holds ground. The scope guard
(`assignments.rs:505-531`) refuses a handoff whose declared scope overlaps any
in-flight task of the same project. A task that declares **no** scope "may touch
anything" and therefore conflicts with every other in-flight task — a plan of
undeclared-scope tasks is effectively serialized to one at a time. Probes `s10`
confirmed this directly.

### Handoffs: attribution, not authority

A handoff row is an audit record plus untrusted context
(`director-domain/src/handoff.rs:5-14`). Accepting one is claim-once and decided by a
conditional `UPDATE ... WHERE state = 'open'` plus a row-count check
(`director-store/src/handoffs.rs:157-180`), and it grants nothing: no assignment, no
status move, no completion.

### Verification is the only path to `done`

Nothing an agent writes makes a task `done`. An agent's own completion report stops
at `verification_pending` (`assignments.rs:1145-1306`); the verdict — and the
status move — is the verification engine's (`verifications.rs:341-401`), applied
under a `state_version` CAS.

---

## 3. Task lifecycle analysis

Statuses: `Backlog → Todo → InProgress → VerificationPending → Done | Failed`,
plus `Cancelled`, `Blocked`.

Transition mechanics, as actually implemented:

| Transition | Writer | Guard |
|---|---|---|
| `todo → in_progress` | `assign_and_start_resolved` (`assignments.rs:828-838`) | **None on status.** `guard_plan_membership` + `guard_scope`, then an unconditional `UPDATE tasks SET status='in_progress'`. The read of the prior status exists only to keep the history honest (`assignments.rs:783-798`). |
| `in_progress → verification_pending` | `report_completion` (`assignments.rs:1222-1232`) | Assignment matched by id **and** `status='active'` (`assignments.rs:1185-1203`); task move carries no CAS. |
| `* → todo` (lease expiry) | `expire_lease` (`assignments.rs:999-1009`) | Active-assignment read inside the tx (`898-907`); task move carries no CAS. |
| `verification_pending → done/failed` | `land_verification` (`verifications.rs:431-443`) | **CAS on `tasks.state_version`.** Zero rows → `StateVersionConflict`, and the whole transaction — verdict row included — rolls back (`verifications.rs:426-430`). |
| any | `update_task` (`tasks.rs:94-99`) | CAS on `state_version` + `row_count_to_outcome`. |

**Observations:**

1. **There is no state machine.** No code anywhere validates that a transition is
   legal (a `TaskStatusTransition` type exists, but only as the read shape of the
   append-only history table; `grep` finds no `is_valid_transition`). The
   transitions that are guarded are guarded by *concurrency* predicates
   (assignment status, task version), not by legality predicates. The status
   machine is a convention of the loop, and the store writes whatever the caller
   passes.
2. `done` is genuinely protected: a `VerificationStatus::task_status()` mapping, a
   CAS, and a rollback that takes the verdict row with it.
3. `assign`'s unconditional status move is the one place "convention" and "write"
   diverge badly — see D1 in §6.

**Versioning.** Every mutable table carries `state_version`; every authority-bearing
update either carries a `WHERE state_version = ?` predicate or bumps the version as
part of a single conditional write. The partial unique indexes are the load-bearing
safety net:

```sql
CREATE UNIQUE INDEX idx_assignments_active_per_task
    ON agent_assignments(task_id) WHERE status = '"active"';
```

Note the JSON-quoted predicate. Status columns store the serde form (`"active"`,
quotes and all), so a predicate written against the bare literal matches nothing and
the index silently goes inert — a trap the schema documents because it was caught by
test, not by reading (`migrations/0001_initial.sql:197-198`, and the same pattern for
checkpoints at `:240-241`).

---

## 4. Concurrency test scenarios — probes written for this audit

New, untracked probe files (no production code was touched):

- `crates/director-store/tests/multi_agent_audit.rs` — 14 probes against the store's
  own primitives, each over a real SQLite file in a temp dir, with races driven by
  real OS threads released by a `Barrier`.
- `crates/director-app/tests/multi_agent_app.rs` — 1 probe at the application layer.

Scenario mapping:

| # | Scenario | Probe | Concurrent? | Result |
|---|---|---|---|---|
| 1 | Two agents claim the same task simultaneously | `s1_two_simultaneous_claims_leave_exactly_one_active_assignment` | 2 threads + barrier | **Pass.** Exactly one active assignment; both tenures recorded; loser released `Reassigned`; no duplicate claim. *But* see D3. |
| 2 | Agent crashes after claiming | `s2_a_crash_after_claiming_is_reclaimed_by_lease_expiry` | crash modeled as abandoned active tenure | **Pass.** Lease expiry releases tenure, task back to `todo`, tenure retained. |
| 3 | Lease expires while agent works | `s3_a_lease_expiring_mid_work_closes_the_session_and_disconnects_the_agent` | — | **Pass.** Session closed `Vanished`, agent `Disconnected`, task `todo`. |
| 4 | Agent submits result after lease expires | `s4_a_result_submitted_after_expiry_does_not_move_the_task` | — | **Pass.** `report_completion` returns `Ok(None)`; task stays `todo`; no verdict; re-assignable. |
| 5 | Old agent updates after another takes ownership | `s5_...` (completion) + `s5b_...` (checkpoint) | — | **Pass.** Late completion is a no-op; a stale-supersession checkpoint write is refused with `StateVersionConflict`. |
| 6 | Two agents update the same task version | `s6_two_writers_on_the_same_task_version_conflict` | 2 stores | **Pass.** Second writer gets `StateVersionConflict { caller: 1, stored ≥ 2 }`. |
| 7 | Handoff submitted twice | `s7_a_handoff_accepted_twice_is_accepted_exactly_once` | 2 threads + barrier | **Pass.** Exactly one acceptance; the refusal is `ConstraintViolation`. |
| 8 | Task recovered after process restart | `s8_a_task_survives_a_restart_and_is_recoverable` | real reopen of the file | **Pass.** Assignment, session, checkpoint visible to the reopened store; tenure reclaimable. |
| 9 | One agent completes while another claims | `s9_...` (sequenced) + `s9b_...` (racing) | 2 threads + barrier | **Fail of the invariant** — see D1, D2. The *probes* pass, recording the behavior. |
| 10 | Multiple independent tasks assigned concurrently | `s10_independent_tasks_are_assigned_concurrently_without_interference` | 2 threads + barrier | **Pass.** Disjoint declared scopes both land; both `in_progress`; one active assignment each. |

Supplementary probes: `a_stale_verdict_does_not_overwrite_a_newer_task_state`
(the verdict CAS), `the_partial_unique_index_refuses_a_second_active_assignment`
(the unambiguity index, probed through the repository insert path).

**Scenario 1 at the application layer** — `two_concurrent_assigns_of_one_task_both_report_success`
(`multi_agent_app.rs`): two threads both call `assign_at` for the *same* task at the
same instant. **Both report success.** The `TaskAlreadyAssigned` refusal that
`assign.rs:312-318` promises never fires, because the check and the write are
separate operations. This is D3.

---

## 5. Test execution results

- **Baseline (before this audit):** `cargo test --workspace` — **1051 passed, 0
  failed**, 76 ignored (live-agent and child-harness entries deliberately excluded
  from ordinary runs). Workspace builds clean.
- **This audit's probes:** `cargo test -p director-store --test multi_agent_audit`
  — **14/14 passed**, run three times consecutively to confirm the race outcomes
  are not timing luck. `cargo test -p director-app --test multi_agent_app` — 1/1
  passed. Re-run after the concurrent commits (`54391ab`, `c3243cf`): still green.
- **Full workspace with the probes present:** everything I touched stayed green.
  One unrelated failure appeared during the audit window:
  `crates/orqynn/tests/security_resume.rs` (test
  `a_repository_supplied_database_executes_its_persisted_check_at_resume`,
  "could not read task SHIP-THE-HELPER-SCRIPT: not found"). That file is an
  **untracked, in-flight addition by a concurrent session** — it appeared
  mid-audit, was never part of the baseline, and fails in isolation (`cargo test
  -p orqynn --test security_resume`), i.e. independent of anything this audit
  added. I left it and its sibling `security_audit.rs` untouched.
- **Production code:** unchanged. `git status` shows the audit's footprint as
  exactly four untracked files: this document, its preserved predecessor, and the
  two probe files.

Two probes were initially written to be *more* assertive than the invariants
deserved, and the races corrected them — worth recording, because it is exactly the
"do not assume a test passes because a unit test covers something similar" trap the
audit brief warns about:

- `s1` first asserted "B holds the task". Under a real barrier the winner is
  nondeterministic; the probe now asserts the invariants (one active tenure, both
  tenures retained, loser `Reassigned`) and pins nothing about who wins.
- `s9b` first asserted "both operations are handled". Under a real race,
  `report_completion` returned `Err(ConstraintViolation("sqlite error 5: database is
  locked"))`. That is D2 below, reproduced deterministically: reruns of `s9b`
  consistently report *interleaving 2*.

---

## 6. Confirmed defects

### D1 — A claim on a completed task drags it back into flight, dodging verification

`assign_and_start` has **no task-status precondition**. Its own comment admits this
("A task that is not `todo` is not started here; the handoff still happens",
`assignments.rs:783-788`), and the app layer's `ready_in_plan` only admits `Todo`
tasks (`assign.rs:263-267`) — so within one well-behaved loop this never fires. But
the store is the authority, and the authority permits it:

- Sequenced (`s9`): after A's completion puts the task at `verification_pending`,
  a claim from any second process moves it to `in_progress` with `displaced ==
  None`. The work A finished now never reaches VERIFY — the verify step surveys
  `verification_pending` tasks only.
- Under contention (`s9b`): whichever order the writes serialize in, the task ends
  `in_progress` under B, and zero verifications are ever recorded.

Severity: **high**. It is a silent completion loss plus a duplicate-work path, and
nothing errors — all parties are told their operation succeeded.

### D2 — SQLite snapshot conflicts are misreported as constraint violations

`expire_lease`, `report_completion`, `acknowledge_assignment`, and the
caller-chosen-id verification path all open **deferred** transactions that read
first and write later. In WAL mode, a deferred transaction that snapshotted before a
concurrent `IMMEDIATE` writer commits is refused at write time with
`SQLITE_BUSY`/`SQLITE_BUSY_SNAPSHOT` — the 5s busy handler does not help, because it
is a snapshot conflict, not a lock wait. (Confirmed empirically: `s9b` fails
sub-second, not after five seconds of waiting.) `translate_error`
(`connection.rs:204-219`) maps every `SqliteFailure` to a single bucket:

```rust
StoreError::ConstraintViolation(format!("sqlite error {}: {detail}", fail.extended_code))
```

so the caller sees "constraint violation" for what is *contention the store
correctly refused*. Reproduced by `s9b`:

```
s9b: interleaving 2 — A's report failed as: ConstraintViolation("sqlite error 5: database is locked")
```

The codebase already knows this class of bug and already wrote the fix — for
checkpoints only. `checkpoints.rs:24-39` documents the identical trap and
`translate_supersession_error` (`checkpoints.rs:281-307`) maps it to
`StateVersionConflict`. The other deferred-transaction sites never got the same
treatment, and nothing distinguishes them except the module they live in.

Severity: **medium**. No invariant is broken — SQLite held the line — but a caller
cannot tell "retry" from "your write was illegal", and the natural handling (treat
`ConstraintViolation` as a data error) is wrong.

### D3 — The app layer's "refuse rather than displace" promise does not survive concurrency

`assign::check_assignment` reads the active assignment and `validate_assignment`
refuses with `AssignError::TaskAlreadyAssigned` when one exists
(`assign.rs:312-318`), documented as "refusing instead means a second handoff would
displace the sitting agent silently". The write it precedes, `assign_and_start`,
always displaces (`assignments.rs:212-249`) — the store's own documentation of the
`next` path says exactly that: *"The outcome for a racing pair is ... the later
handoff displaces the earlier one ... Nothing is refused"*
(`assignments.rs:739-743`).

So the two layers document opposite semantics for the same operation, and the
observed behavior is the store's: `two_concurrent_assigns_of_one_task_both_report_success`
shows both callers receiving `Ok`, the first assignment silently revoked.

Severity: **medium**. The hard invariant (one active assignment per task) holds —
the partial unique index decides it, not the pre-check. What breaks is the softer
contract a scheduler might rely on ("assignment is exclusive until released"), and
the audit trail's honesty about *why* a tenure ended (a lost claim race is recorded
as `Reassigned`, which reads as deliberate).

### D4 — Report/expire task moves carry no version guard

`report_completion` and `expire_lease` move the task row with
`UPDATE tasks SET status = ? WHERE id = ?` and no `state_version` predicate
(`assignments.rs:1222-1232`, `assignments.rs:999-1009`). Their safety rests entirely
on the assignment-status guard earlier in the same transaction. That guard is sound
in every interleaving this audit could produce — but it is a *shared-snapshot*
argument, not the explicit CAS every other authority write carries. See §7.

### D5 — App-layer pre-checks that the store does not re-validate

Confirmed by reading `assign.rs:186-225` against `assignments.rs:761-862`: the
store re-validates exactly two eligibility inputs under its write lock — plan
membership and scope. Everything else — task status `todo`, agent availability,
agent capabilities, dependencies done — is an optimistic pre-check that a racing
writer can invalidate. D1 is the material consequence.

---

## 7. Potential race conditions (not confirmed, but the window is real)

- **The agent-status window.** Nothing in the assignment transaction checks
  `agents.status` or `current_task`. An agent that went `Offline` (operator) or
  already holds another task between `check_assignment` and the commit still
  receives the assignment, and `handoff`'s `current_task` write is unconditional
  (`assignments.rs:330-335`). The one-task-per-agent rule is enforced only inside
  SCHEDULE's in-round accounting (`schedule.rs:161-235`: `committed` set plus
  `current_task` filter), which is per-process and per-round. Two concurrent
  SCHEDULE rounds, or a schedule round alongside a direct assign, can commit one
  agent to two tasks.
- **D4's window:** a verdict (`apply_verification_with_actions_next`, IMMEDIATE,
  CAS-guarded) racing `report_completion`'s unguarded task move. Both are
  serialized by the write lock, so no lost update is reachable through the
  experiments run here; the residual question is whether *recovery* code that
  re-reads the task mid-window can act on a status that is about to change.
- **Checkpoint writes are not tied to tenure.** `create_checkpoint` has no
  assignment check of any kind. A *stale* agent (one whose read predates the
  displacement) is refused by the version gate — `s5b` confirms this — but a
  stale agent that re-reads can supersede the current holder's checkpoint. Nothing
  in the store prevents it; only the MCP adapter's session wall does.
- **Pool exhaustion aborts the process** rather than queuing
  (`connection.rs:126-136`). Under a fan-out big enough to hold 8 connections
  simultaneously, the loop panics instead of degrading. Deliberate, but worth
  knowing before scaling agent count.

---

## 8. Missing tests

From the audit's own probe coverage relative to the ten required scenarios, and from
reading the existing suites (`assign_concurrency.rs`, `verification_atomicity.rs`,
`runtime_recovery.rs`, `process_kill.rs`, `store.rs`, `handoffs.rs`,
`displacement_*.rs`):

1. **A genuine same-task claim race.** Every existing concurrent-assignment test
   races *two different tasks with overlapping scopes*; none races two agents on the
   identical task. The uniqueness is protected by a partial unique index whose only
   sequential test is in `store.rs`. Fixed by `s1`.
2. **A claim racing a completion (scenario 9).** Absent from the whole suite.
   Nearest coverage (`assign_concurrency.rs:862`) reassigns an in-flight task, which
   is a different transition. Fixed by `s9`/`s9b` — which is how D1 and D2 surfaced.
3. **A verification CAS racing an assignment's status move.** Covered in one
   direction (`verification_atomicity.rs:531`); not against an *unguarded* status
   writer like `report_completion`.
4. **Contention-error classification.** Nothing asserts what a caller sees when a
   deferred transaction loses its snapshot. The suite asserts outcomes, never the
   *error class* under contention. `checkpoints.rs` has its own translator with no
   test for the sibling sites.
5. **Scheduling under concurrency.** No test exercises two SCHEDULE rounds against
   one another, or SCHEDULE against a direct `assign_at` — the interleaving §7 flags
   for the one-agent-many-tasks invariant.
6. **The app-layer refusal contract.** `assign.rs`'s `TaskAlreadyAssigned` behavior
   is tested only sequentially. Fixed by `multi_agent_app.rs`.

The suite's strongest existing work — `process_kill.rs` (real child-process kills
and cross-process races), `verification_atomicity.rs`, `runtime_recovery.rs` — was
confirmed honest: genuinely concurrent where it claims to be, and explicit about
where it is a sequential simulation.

---

## 9. Recommended fixes

Ordered by risk-to-reward; each is small and local.

1. **Close D1:** give `assign_and_start_resolved` a status precondition — refuse
   with a structured `StoreError` (e.g. `TaskNotAssignable { status }`) unless the
   task read inside the transaction is `Todo` or the incoming write is a legitimate
   reassignment of an in-flight task the caller can name. This is one guarded
   `UPDATE` and a structured error, not a redesign: `guard_scope` and
   `guard_plan_membership` are the template.
2. **Close D2:** lift `translate_supersession_error`'s mapping out of
   `checkpoints.rs` into `connection.rs` as a general "deferred-write contention"
   translator, and use it at the commit of `expire_lease`, `report_completion`,
   `acknowledge_assignment`, and `apply_verification_resolved`. The pattern already
   exists; this is applying it.
3. **Close D3:** decide one semantics. Either (a) make the store's `handoff` refuse
   a sitting tenure when the caller passes an explicit "must be unassigned" flag —
   `assign_at` wants this, `assign_and_start_next` does not — or (b) change the
   `assign.rs` documentation to say displacement is possible under concurrency.
   Option (a) is a predicate and an error variant; it also makes D1's fix cheaper.
4. **Close D5's agent half:** re-check `agents.status`/`current_task` inside the
   assignment transaction, the way `guard_plan_membership` re-checks the plan. This
   is also what makes the one-task-per-agent rule durable rather than conventional.
5. Add `state_version` predicates to the D4 task moves, or document at each site
   why the assignment-status guard is complete.
6. Add the tests listed in §8 as regression tests, and fold the two probe files
   into the suite (with the `s9b` interleaving recorded rather than pinned).

None of these require the schema to change, and none change any public signature.

---

## 10. Final reliability verdict

**The hard invariants hold. No unauthorized duplicate claim, no stale-agent
overwrite of newer task state, no duplicate completion or verification, no lost
committed state, and no ownership bypass were found at the layer that decides them.**

Specifically, the audit set out to verify six properties and found every one of them
enforced at the correct layer:

- **No duplicate claims** — the partial unique index plus release-before-insert in
  one `IMMEDIATE` transaction. Verified by direct index probe and by a barriered
  two-thread race.
- **No stale overwrite of newer state** — `state_version` CAS on every
  authority-bearing task write, with `row_count_to_outcome` making zero rows a
  conflict rather than a no-op.
- **No invalid transition** — *not* enforced as a state machine (there is none), but
  the transitions that matter (`done` especially) are unreachable except through the
  verdict CAS.
- **No lost committed state** — append-only assignment history, never-deleted
  checkpoints, and restart recovery verified against a real reopened file.
- **No duplicate completion or verification** — tenure-matched completion refuses
  released tenures; the verdict CAS rolls back a stale round's verdict row with its
  status move.
- **Correct under SQLite contention** — the write lock is taken before the reads
  that decide the write (`BEGIN IMMEDIATE`) at exactly the places that need it, and
  contention *waits*. With one exception: **D2**, where contention is survived but
  misreported.

**Two genuine defects were found and reproduced, not theorized:** D1 (a completed
task can be silently returned to flight, where it will never be verified) and D2
(contention is reported to callers as a constraint violation). Both are in code the
existing suite covers well for the single-director case, which is precisely why they
survived: the suite's concurrency tests are excellent at *scope* races and *process*
crashes, and thin at *claim-vs-completion* races and *error classification* under
contention. D3 is a documentation-vs-behavior contradiction with a soft impact, and
D4/D5 are hardening items rather than live failures.

**Verdict: safe to operate as a single director on one database; safe to restart and
to run competing writers; not yet safe to treat as a multi-writer system whose errors
are correctly classified, and not yet protective of a completed task against a late
claim.** The fixes in §9 are local and would close the gap.

---

## Next recommended phase

**Phase 43: claim legality and contention classification — the two defects this
audit reproduced.**

Exact reason for selecting it: the audit was scoped to *find* concurrency defects,
and it found exactly two that are reproduced by tests rather than inferred (D1, D2),
plus a third that is a live contradiction between two layers' documentation (D3).
Phase 42 produced the failing-behavior evidence and the fix shapes; the next phase
should convert them into guarded writes, because both defects are in the
assignment/lease path that every multi-agent deployment depends on, and both are
small enough to land without touching the schema or any public API. Recommendation 2
(§9) is the cheapest first step — the fix already exists as a local pattern in
`checkpoints.rs` — followed by recommendation 1, for which `guard_scope` is the
template.
