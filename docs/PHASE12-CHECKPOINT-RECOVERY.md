# Phase 12 — Automatic checkpointing and recovery context

> **Status:** implemented in the working tree, **uncommitted**, and currently
> **not compiling**. See [The unfinished edit](#the-unfinished-edit) before
> building anything from this document.

Phase 11 proved the store alone is enough to brief a replacement agent mid-flight —
and it proved it the hard way. Its context-continuity test,
`a_replacement_agent_can_be_given_everything_the_prior_agent_left_behind`, issued six
unrelated repository queries and assembled the picture by hand. The test said plainly
what was missing: no step in the loop wrote the checkpoint it then read back.

Phase 12 removes both halves of that gap.

1. **[`director-app::checkpoint`] writes checkpoints automatically**, inside the
   observation round, from evidence only that round holds.
2. **[`director-app::recovery`] assembles the picture in one place**, and
   [`schedule`] hands a replacement agent its briefing at the moment the scheduler
   gives it the work.

A replacement — or a driver briefing one — now asks one question and gets one answer.

```text
RecoveryBuilder::new(store, task_id)
   ↓  .observed(git_state, git_history)   // or .from_stored_state()
   ↓  .without_histories()                // optional
   ↓  .build()
RecoveryOutcome::Built(recovery) | UnknownTask | NoCheckpoint | NoProject | NoStoredProjectState
   ↓
ResumeStatus::Ready | Stale | Conflicted | Blocked | Insufficient
```

## Why the checkpoint writer lives in OBSERVE

The loop has exactly one place that looks at a repository and records what it found:
`observe`. Checkpoint content is *evidence* — the branch, the commit `HEAD` points at,
the files that changed. OBSERVE already holds all of it, and it holds the
[`SyncResult`] that says whether anything actually moved. MONITOR, the other candidate,
works off heartbeats and assignments and has no git state at all; a checkpoint it wrote
would be a narrative with no evidence behind it, which is precisely the self-reporting
shape the loop's invariants exist to prevent.

## What is checkpointed, and what is not

Only tasks that are `TaskStatus::InProgress` are checkpointed, and only when the
observation recorded a *real* change — a moved `HEAD`, a moved branch, or a changed
working tree. A quiet round writes nothing, because a checkpoint that restates the
previous one is noise in the recovery lineage without adding any information. Tasks that
are `todo`, `verification_pending`, `done`, `failed`, or `cancelled` are not in flight,
so there is no resumption document to write for them.

`should_checkpoint` is the whole predicate. `restates_current` is the guard that keeps a
re-observation of an unchanged tree from duplicating the lineage.

### Lineage-derived ids, not generator counters

The checkpoint's id is its *position in the task's lineage*: one past the checkpoint the
round read as its write precondition, giving `CHK-<task>-3` for the third checkpoint of a
task. It is derived from the lineage rather than from `IdGenerator` because the lineage
lives in SQLite and the generator does not. `checkpoints.id` is a `PRIMARY KEY`, so an id
minted from a fresh generator's counter — which is exactly what a restarted process has —
would collide with the checkpoint a previous process already wrote, and the store's
constraint would refuse the write and take the whole observation round down with it.

That is not hypothetical. `zz_scratch_verify.rs` exists to hold the regression:
`a_restart_advances_the_lineage_instead_of_colliding`.

### What the checkpoint does not contain

Every field is derived from records Orqyn holds. `progress` is a *description of observed
state*, not an agent's claim — it reports what git shows and never says a task is
complete. Nothing here can reach `TaskStatus::Done`; that is still reachable only through
independent verification.

Fields Orqyn cannot fill honestly are left at their defaults rather than invented:
`test_results` is `None` because test outcomes are produced by verification, not
observation; `current_blocker` is `None` because `Blocker` is not persisted;
`important_decisions` is empty because a decision is recorded by a caller's explicit
choice, not inferred from a diff.

### Best-effort semantic memory

A checkpoint is committed to SQLite first, and only then published to the semantic-memory
substrate. SQLite is authoritative, so a substrate that cannot answer in bounded time
must not hold the observation round open. `MEMORY_PUBLISH_TIMEOUT` is ten seconds; a
timeout costs the recovery lineage nothing, because the checkpoint is already committed.
The failure is recorded in `CheckpointRound::memory_failures`, with the substrate's own
error text, and the next round publishes the page then.

## The recovery context

`RecoveryBuilder` is the seam. `recovery_context` and `recovery_context_from_store` are
thin wrappers over it that keep the single-question shape [`schedule`] already has; the
builder is for a caller that needs to choose the observation source, skip the history
queries, or see *why* nothing was built.

The result is `RecoveryOutcome`, which reports absence with the same precision it reports
success:

| Outcome | Meaning |
|---|---|
| `Built(Box<Recovery>)` | A package was assembled. Its `gaps` record what could not be reconstructed. |
| `UnknownTask(id)` | Orqyn has no record of this task. A caller mistake. |
| `NoCheckpoint(id)` | The task exists but was never checkpointed. Its history is answerable; resumption is not. |
| `NoProject(id)` | The task is not claimed for any project, so there is no state to compare against. |
| `NoStoredProjectState(id)` | The task's project was never observed. |

### The resume verdict

`Checkpoint::compare_with` is the guard that has existed since Phase 5 and had no caller.
It now has one. The checkpoint records the commit and branch it was taken against; the
recovery reads the project's state and asks whether that world still exists.

- **`Ready`** — the checkpoint is current, the context is live, nothing is blocked.
- **`Stale`** — the checkpoint describes a world that has moved on. Not an error: the
  honest answer, telling the replacement to reconcile rather than trust the narrative.
  A dirty tree on the same commit, an advanced `HEAD`, a rebased-away commit, and a
  branch change are all `Stale`.
- **`Conflicted`** — two sources disagree about the task's state. A human or the planner
  must adjudicate.
- **`Blocked`** — reported before staleness, because a blocked task is blocked in every
  world.
- **`Insufficient`** — no checkpoint, no agent, or missing project state.

### Honest gaps

Two fields of a `ContinuationPackage` cannot be populated from anything Orqyn stores.
Rather than invent content for them, both are represented at their empty defaults with
the reason recorded in `RecoveryGaps`:

- **`RecentContext`** has no table and no repository. It is an in-memory action window;
  persisting it is out of scope for Phase 12. The package carries an empty window and
  `Recovery::actions_available()` returns `false`.
- **`Blocker`** is not persisted either. REPLAN reports stranded dependents and writes
  nothing about them, so the blocker list is empty — the *task's* status still carries
  `blocked`, which is what drives `ResumeStatus::Blocked`.

A gap is a statement about Orqyn's persistence, never about the work: an empty window
with `recent_context_unavailable` set means Orqyn does not keep that window, not that the
agent did nothing. Without the flag the two read identically. This is why
`RecoveryGaps::is_clean()` is `false` for every build today.

**No new tables were added to make the package look complete.** A field that cannot be
reconstructed is a field reported as absent, not a field filled with a guess.

## Where SCHEDULE hands it over

The recovery context is assembled *inside* the scheduling round, not asked for
afterwards. When the round pairs a task with an agent and the task already has a
checkpoint, the pairing carries `Scheduled { recovery: Some(Recovery) }`:

- A **fresh** task reads no checkpoint and yields `None` — there is nothing to inherit.
- A **resumed** task arrives with its briefing attached, so a replacement agent is
  handed the context at the moment it receives the work instead of having to ask six
  unrelated repository questions itself.

This is the Phase 12 continuation package. It does not replace the assignment, and it
does not reach past `assign`'s legality checks — the scheduler composes `assign`, and the
recovery rides along on a pairing the store has already accepted.

## Test coverage

Unit tests in `director-app::checkpoint` and `director-app::recovery`, plus two
integration suites.

`tests/recovery_builder.rs` (18 tests) drives the builder directly, including the
absence cases:

- `a_task_orqyn_never_recorded_reports_unknown_task`
- `a_task_with_no_checkpoint_reports_no_checkpoint`
- `a_task_with_no_checkpoint_and_no_observation_reports_the_missing_state`
- `a_task_whose_project_was_never_observed_reports_no_stored_state`
- `an_in_flight_task_on_the_checkpointed_commit_builds_ready`
- `a_dirty_tree_on_the_same_commit_is_stale`
- `an_advanced_head_is_stale`
- `a_rebased_away_commit_is_stale`
- `a_branch_change_is_stale`
- `a_blocked_task_reports_blocked_before_staleness`
- `a_done_task_with_no_passing_evidence_is_conflicted`
- `a_done_task_whose_checkpoint_records_passing_tests_is_not_conflicted`
- `the_package_carries_the_persisted_histories`
- `without_histories_builds_a_verdict_with_empty_history_vectors`
- `unpersisted_context_is_reported_as_unavailable_not_as_empty`
- `the_package_reports_the_task_version_it_was_built_against`

`tests/runtime_recovery.rs` (5 tests) covers the persistence properties — restart,
abandonment, a vanished agent losing its lease to a replacement, rework after failure,
and context continuity.

## The unfinished edit

The working tree does not compile. `CheckpointRound` gained a documented `conflicts`
field — a round that loses a race for one task's checkpoint lineage should report it and
keep checkpointing the others, not abort — and the field, the `CheckpointConflict` type,
its `retry_against()` helper, and the `has_conflicts()` / `conflicted_tasks()` accessors
all exist and are documented. But `checkpoint_round` still propagates a conflict with `?`
and omits the field from its constructor, so nothing ever pushes into `conflicts`:

```
error[E0063]: missing field `conflicts` in initializer of `CheckpointRound`
  --> crates/director-app/src/checkpoint.rs:527
```

The completion is small and the shape is already dictated by the existing types. In
`checkpoint_round`, around the `create_checkpoint` call: catch
`StoreError::StateVersionConflict { stored, caller, .. }` per task, push a
`CheckpointConflict { task_id: task.id.clone(), stored, caller }`, `continue`, convert
any other error with `.into()`, and include `conflicts` in the returned `CheckpointRound`.
`CheckpointError::Conflict` in `director-app::lib.rs` is the variant this replaces —
observe already maps it to `ObserveError::Checkpointing`, so once the round collects
instead of propagating, a caller learns about a lost race through the round's report
rather than through a failed round.

`retry_against()` carries the cure with the complaint: `stored` is the version the store
reported when it refused, so a caller that re-derives its checkpoint and presents that
version writes at the next lineage position rather than conflicting a second time.

## What Phase 12 does not do

- **It does not add a process.** There is still no daemon, no driver, and no MCP server.
  `director-app` remains a library whose caller is the tests.
- **It does not persist `RecentContext` or `Blocker`.** Both are reported as gaps rather
  than silently empty.
- **It does not resume an agent automatically.** It hands a driver or a replacement the
  picture; deciding what to do with it is still a caller's job.
- **It does not change the verification invariant.** A checkpoint's `progress` field
  describes observed state and can never mark a task complete.

## Relationship to Phase 11

Phase 11's limitations section says *"No loop step writes a checkpoint… the progress
narrative has to be written by the caller until that step exists."* That was accurate when
it was written and this phase is the step it was waiting for. Phase 11's five tests are
unmodified; the hand-assembled context test is now backed by a writer, and its six
queries have a home in `recovery`.

[`director-app::checkpoint`]: https://github.com/abyyxhek/orqynn/blob/main/crates/director-app/src/checkpoint.rs
[`director-app::recovery`]: https://github.com/abyyxhek/orqynn/blob/main/crates/director-app/src/recovery.rs
[`schedule`]: https://github.com/abyyxhek/orqynn/blob/main/crates/director-app/src/schedule.rs
[`SyncResult`]: https://github.com/abyyxhek/orqynn/blob/main/crates/director-domain/src/repository.rs
