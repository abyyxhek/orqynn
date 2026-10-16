# Phase 13 — Deterministic Conflict Detection

**Status: complete.** Normalized `scope_paths` compared as path prefixes; SCHEDULE refuses
overlapping pairings and ASSIGN refuses the handoff. An undeclared scope is treated as
unrestricted, not empty.

**What this phase is for.** Two agents editing the same files is the failure mode every other
part of Orqynn exists to prevent, and nothing detected it. A plan could name `src/auth` and
`src/auth/login.rs` as two independent tasks, the scheduler would hand both to different
agents in the same round, and the result would be a merge neither agent could have prevented.

---

## 1. What was already there, and why it was not enough

`Task::scope_paths` existed, was persisted, was read back, and round-tripped over the MCP wire.
So did `Task::overlaps_scope`, with three passing unit tests. The feature was nevertheless inert:

| Gap | Evidence before this phase |
|---|---|
| The predicate had **no production caller** | `overlaps_scope` appeared at `task.rs:296` (the definition) and four times inside `#[cfg(test)]`. Nothing in `assign.rs` or `schedule.rs` referenced scope at all |
| **No way to declare a scope** | `TaskSpec` had `new`, `depends_on`, `expects`, `sized` — and no scope builder. Every planned task was persisted with `scope_paths: vec![]` |
| The predicate was **wrong** | Root scope `/` normalized to the empty string and therefore never overlapped anything; empty scopes read as "conflicts with nothing"; `\` was not handled; `..` and `.` were not resolved; `src/auth` and `src/author` were treated as overlapping by string prefix |

The predicate being correct but uncalled is the specific shape of bug this project has hit
before — the `plan.rs` file that "existed" for days without being declared as a module. That is
why this phase is verified in `tests/` against the real steps rather than only in `src/`.

---

## 2. Scope semantics

### 2.1 Normalization

`director_domain::task::normalize_scope_path` reduces one declared path to the components
Orqynn compares. Every rule has a test.

| Rule | Why |
|---|---|
| Blank is rejected | A whitespace-only entry names nothing |
| Control characters are rejected | A NUL or newline is not a path; it is something that arrived from outside, and it would corrupt a log line |
| `\` becomes `/` | The same tree is written both ways, and Orqynn is developed on Windows. Without this, `src\auth` and `src/auth` read as disjoint trees |
| Empty segments are dropped | `a//b` and `a/b` are one path |
| `.` is dropped, `..` pops | `a/./b` is `a/b`; `a/b/../c` is `a/c`. Leaving `..` unresolved lets two spellings of one directory disagree |
| `..` cannot escape a **relative** path | `../src` names ground *outside* the project, which Orqynn is not managing. **Rejected, not clamped** |
| `..` clamps at an **absolute** root | `/..` is `/` by definition |
| A leading `/` is dropped for comparison | `/src/auth` and `src/auth` overlap. Conservative on purpose: a false conflict costs one round, a miss costs a merge |
| Case is preserved | See §2.4 |

A path reducing to zero components is the **repository root**, and is deliberately not `None`:
an empty component list is a prefix of everything, which is exactly what `/` should mean.

### 2.2 Overlap

`Task::scope_overlap` returns `None`, or a `ScopeOverlap` naming the colliding declarations.
Two declared scopes overlap when **one is a path prefix of the other** after normalization. One
rule covers every case that matters:

| Case | Example | Overlaps |
|---|---|---|
| Identical scopes | `src/auth/` vs `src/auth/` | yes |
| Parent / child | `src` vs `src/auth/login.rs` | yes |
| Same file named twice | `src/auth/login.rs` vs itself | yes |
| Different directories | `src/auth/` vs `src/db/` | no |
| Shared string prefix, different directories | `src/auth` vs `src/author` | **no** — a plain `starts_with` gets this wrong |
| Repository root | `/` vs anything | yes |

Overlap is **conservative**: a directory scope conflicts with work inside it even when the two
touch disjoint files. A false conflict costs one scheduling round; a missed one costs a
corrupted merge. Semantic conflict detection — reading files and reasoning about them — is out
of scope for this version.

### 2.3 An undeclared scope is not an empty scope

**This is the decision the whole phase rests on.**

A task with no `scope_paths`, or only unusable entries, has an **undeclared** scope: Orqynn does
not know what it may touch. That is treated as overlapping **everything**, including another
undeclared scope.

The rejected alternative — reading an empty list as "no constraint, safe to run beside anything"
— would make `Task::new()`, the constructor every task starts from, silently unsafe. The cost of
the chosen rule is real and worth stating plainly:

> **A caller who declares no scopes gets no concurrency at all.** Declaring disjoint scopes is
> what buys parallelism back.

This is a behaviour change, not a new capability. Before this phase, two unscoped tasks were
scheduled together; now they are refused. Four existing tests in `tests/schedule.rs` relied on the
old behaviour and now declare a disjoint scope per fixture — which is what those tests meant to
say, since they are about pairing tasks with agents rather than about scopes.

### 2.4 Case sensitivity, and the limitation it creates

Comparison is **case-sensitive on every platform**. Orqynn is deciding whether two *declared*
scopes cover the same ground; it is not asking the filesystem. Lower-casing only on Windows and
macOS would make the same plan produce different assignments on different machines, and
deterministic cross-platform behaviour was judged more valuable than closing the gap below.

**Known limitation.** On a case-insensitive filesystem, `src/Auth` and `src/auth` are one
directory, and Orqynn will not see that. Two tasks so declared could still collide. This is
pinned by `scope_comparison_does_not_vary_with_case` so it cannot change by accident. The fix, if
wanted, is a per-project case-sensitivity setting rather than a platform conditional.

---

## 3. Where scopes come from

`TaskSpec::scopes(paths)` is the single path from a caller to a persisted scope:

```rust
TaskSpec::new(TaskId::from_string("AUTH"), "Define auth schemas", "POST /login returns 200")
    .scopes(["src/auth/"])
```

`plan_at` copies it onto the task (`plan.rs`), which the store already persists in
`tasks.scope_paths`. **No new crate, no new migration, and no new store method were required** —
`TaskRepository::list_tasks` already returns full tasks including their scopes.

A task with no declared scope is persisted with an empty list, exactly as before. Nothing about
the storage format changed.

---

## 4. Where conflicts are refused

### 4.1 ASSIGN — the authority

`assign::validate_assignment` gained one check, placed after the dependency check and before the
agent checks. It runs against `plan_tasks`, the list `assign_at` reads from the store **on every
call**:

```rust
if let Some(overlap) = scope_holder(task, plan_tasks) {
    return Err(AssignError::ScopeConflict { task, holder, overlap, undeclared });
}
```

- Only tasks `InProgress` hold ground. A `todo` sibling holds nothing — treating it as a holder
  would deadlock the scheduler, since nothing could ever start. Terminal work is over.
- The task list is **live**, so a task that became `in_progress` after planning is caught rather
  than assumed away. This is the revalidation requirement, and it is already satisfied by the
  existing read; no caching was introduced that could go stale.
- The refusal happens **before any write**, so a refused handoff leaves the task `todo` and
  writes no assignment row.
- The search is in `plan_tasks` order, so when two in-flight tasks both collide the one reported
  is deterministic.

`AssignError::ScopeConflict { task, holder, overlap, undeclared }` names both the task in the way
and the paths in dispute. `overlap` is a rendered string; `undeclared` distinguishes "these paths
collide" from "one task said nothing, so it may touch anything".

### 4.2 SCHEDULE — contention within a round

`assign_at` compares one task against what is already `in_progress` when it is called. It cannot
see two tasks in the *same* round, because the first is written before the second is proposed.
So the round tracks what it has already handed out:

```rust
let mut committed_scopes: Vec<Task> = Vec::new();
// ...per task, before anything else:
if let Some(holder) = committed_scopes.iter().find(|other| task.scope_overlap(other).is_some()) {
    report.unassigned.push(Unassigned { task_id, reason: UnassignedReason::ScopeConflict { holder } });
    continue;
}
```

Checked **first**, so a conflict is reported as a conflict rather than arriving later as an
ASSIGN refusal a caller would have to know to look for.

`UnassignedReason` lost its `Copy` derive — `ScopeConflict` carries a `TaskId`. It is `Clone`,
and `ScheduleReport::unassigned()` clones the reason out. Only the conflict variant allocates, and
only when there is a real conflict to report.

### 4.3 Two channels, deliberately

A conflict can surface as `ScheduleReport::unassigned` (nothing was handed out this round) or as
`ScheduleReport::refused` (a pairing was proposed and the store denied it). Which one depends on
whether an agent was free:

- two agents, overlapping scopes → the round refuses to pair the second → `unassigned`;
- one agent, a task already in flight → the pairing is proposed, `assign` denies it → `refused`.

Both are structured and both name the conflict. Neither drops the task silently, and both make
`is_settled()` false so a caller sees work waiting.

---

## 5. Verification

### 5.1 Gates

| Gate | Result |
|---|---|
| `cargo fmt --all --check` | exit 0 |
| `cargo build --workspace` | exit 0, zero warnings |
| `cargo test --workspace --all-targets` | **639 passed, 0 failed, 13 ignored** |
| `cargo test --workspace --doc` | 2 passed, 0 failed |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0, zero warnings |

**Total 641 passing** (639 + 2 doctests), up from 608. The 13 ignored tests are the live MCP
suites, which spawn `handoff-mcp` and `ai-memory` binaries absent from this environment.

### 5.2 Test inventory — 33 new tests

**Domain unit tests** (`crates/director-domain/src/task.rs`, +19): identical scopes; parent/child;
non-overlapping directories; shared string prefix that is not a path boundary; same file twice;
root scope; undeclared vs declared; undeclared vs undeclared; unusable entries; Windows
separators; `.` / `..` / `//` / whitespace spellings; relative traversal rejected; absolute
clamping; control characters; absolute-vs-relative collision; case-insensitivity pinned; self
exclusion; `normalize_scope_paths` returning `None`; serde round-trip.

**Integration tests** (`crates/director-app/tests/conflict.rs`, new file, +14): scope survives
PLAN; scopes accumulate; disjoint scopes both scheduled; overlapping scopes not both handed out;
the conflict names the holder; root-scope task left alone; two unscoped tasks do not run together;
ASSIGN refuses an overlap; refusal writes nothing; ASSIGN refuses undeclared against in-flight;
conflict clears once the holder finishes; a `todo` sibling holds nothing; **a task that started
after planning is caught at assignment time**; a second round finds the conflict the first
created.

Every integration assertion is produced by PLAN, SCHEDULE, or ASSIGN against a real SQLite store.
No test pokes a status by hand to set up its assertion.

### 5.3 The tests fail when the feature is removed

The completion criterion for this phase was that the checks must run in production, not merely
exist. Verified by neutering both production call sites and re-running:

| Configuration | `tests/conflict.rs` |
|---|---|
| Both call sites live | **14 passed, 0 failed** |
| Both call sites neutered | **3 passed, 11 failed** |

The three survivors are the ones that do not depend on the check: scope survives PLAN, scopes
accumulate, disjoint scopes are both handed out. Everything that asserts a refusal fails.
Restored and re-verified at 14/14.

---

## 6. What this phase deliberately does not do

- **No semantic conflict detection.** Overlap is lexical. Two tasks that touch disjoint files
  under one directory are refused anyway, by design.
- **No case-insensitive matching on Windows/macOS.** See §2.4 — a deliberate trade for
  cross-platform determinism, with the cost stated.
- **No `Blocker` persistence.** `BlockerKind::Conflict` exists and a `blockers` table does not.
  Raising a durable blocker on a refused overlap would be the natural follow-up, and would need
  migration `0004`; it was left out here because conflict detection does not require it and the
  phase was scoped to detection and refusal.
- **No migration.** `tasks.scope_paths` already existed.
- **No store method.** `list_tasks` already returned the scopes.
- **No task decomposition.** PLAN still receives its `TaskSpec`s from the caller; it validates and
  orders, it does not decide what the tasks are.
- **The continuation package does not carry `scope_paths`.** `ContinuationPackage` has 15 fields
  and none of them is the task's scope, so a replacement agent briefed by `RecoveryBuilder` learns
  its contract, its history, and its next action — but not what it is allowed to touch. The scope
  is still enforced by ASSIGN and SCHEDULE, so this is a completeness gap in the briefing, not a
  hole in the enforcement. Adding the field is a natural follow-up to Phase 12.
- **No detection of collisions that appear *during* a round**, between an agent's write and the
  next round's read. MONITOR can surface an overlap but does not yet act on one.
- **No protection against two *simultaneous* assigns.** See §7 for the exact transaction
  boundaries and the minimal fix.

---

## 7. Residual risks

1. **Undeclared scopes serialize concurrency.** Every caller that has not adopted `.scopes(...)`
   now gets no parallelism. This is the intended trade (§2.3) and is loud, but it is the single
   biggest behavioural change in this phase and should be called out in release notes.
2. **Case-insensitive filesystems.** Two scopes differing only in case can still collide (§2.4).
3. **Conservative directory scopes.** `src` and `src/auth` are refused together even when they
   touch disjoint files, which costs scheduler throughput on coarse declarations.
4. **Cross-project scope comparison is not implemented.** `assign_at` lists tasks for the task's
   project only, so two projects rooted in overlapping paths would not see each other. Correct
   for now — a project is a repository root — but worth stating if Orqynn ever orchestrates
   several checkouts of one repository.
5. **No in-round detection of a write that appears mid-round** beyond what the loop's sequential
   structure provides.
6. **Two simultaneous `assign` calls can both win.** `validate_assignment` reads the task list
   *before* `assign_and_start` opens its transaction, so two concurrent calls for two overlapping
   tasks can each observe "nothing in progress" and each commit. The store's partial unique index
   guards one active assignment *per task*, not one holder *per scope path*. Today's loop is
   sequential so this is latent, not live — but it becomes real the moment ASSIGN runs
   concurrently, which is what a daemon would do. Minimal fix and the tests that would prove it
   are in `PHASE13_AUDIT_NOTE.md` §3.
7. **A replacement agent's briefing omits its scope.** `ContinuationPackage` does not carry
   `scope_paths`. Enforcement is unaffected; the briefing is incomplete.
