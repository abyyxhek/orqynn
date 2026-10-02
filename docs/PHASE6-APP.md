# Phase 6 — The Control Loop, and Its First Steps

> **Status: underway.** The `director-app` crate exists and its first four
> steps — OBSERVE, PLAN, ASSIGN, and MONITOR — are landed and tested. The
> remaining steps are built next, one at a time, in the order they run.

## Goal

Phases 1–3 and 5 built layers, each verified against its own tests and nothing
else: the domain model, the substrate adapters, the git observation layer, the
store. That was deliberate — each one had to be right on its own terms — but
it also means no code anywhere in the workspace had ever *composed* them.
Nothing opened a store, looked at a repository, and recorded what it found.

Phase 6 is the crate that does. `director-app` is Orqyn's control loop:

```text
OBSERVE → PLAN → ASSIGN → MONITOR → VERIFY → REPLAN
```

The crate implements that loop one step at a time, and each step is landed and
tested before the next is written — the loop is built in the order it runs, so
a step is never written against steps that do not exist yet. OBSERVE is first
for a structural reason: it is the only step whose inputs come entirely from
outside Orqyn. PLAN reads the state OBSERVE produced; ASSIGN reads what PLAN
decided; MONITOR and VERIFY read what ASSIGN started. Every step after the
first consumes the output of the one before it, so writing OBSERVE first is
what gives the rest something to be tested against.

## OBSERVE: evidence becomes belief

One round of `observe()` composes two layers that Phase 2 and Phase 5 built
independently:

- **`GitService`** (director-adapters) reads the live repository through git2
  and persists the *raw* observation — the repository record, its snapshot,
  and the event log — under a JSON store. That is what makes change detection
  possible: it remembers the last commit it saw.
- **`Store`** (director-store) holds Orqyn's *normalized belief* about the
  project in SQLite. That is what planning and assignment will read.

The two are deliberately not the same thing. The git observation is evidence:
what git said, at a time, verbatim. The normalized state is a belief Orqyn
holds between observations, in a shape chosen for its own decisions rather
than for fidelity to git. Writing one from the other is the whole job of the
step, and it is the first code anywhere in the workspace that opens a `Store`.

A round does four things: ensure the project exists (idempotent, so a second
tick is not an error); register the repository if it has never been observed —
not an optimization, since an unregistered repo has no stored snapshot and
therefore no baseline to compare against; sync the repository against git,
which is the actual observation; and fold the resulting snapshot into a
`StoredProjectState` and write it to the store.

## Two design points that only look small

**`state_version` is a write counter, not a change signal.** The store bumps
it on every write, so a version comparison would report every observation as a
change. The authoritative answer to "did anything change" is the git layer's
`SyncResult`, which compares the snapshot against the previous one
deterministically. The loop branches on that, never on the version.

**`first_observation` comes from the registration, not from the sync status.**
`SyncStatus::Baseline` fires only for a repository with no commits to walk, so
a repository that already has history syncs as `Synced` on its very first
observation. The loop's later steps branch on arrival-versus-change, and the
arrival signal is "the registration happened at all", so `observe` reports it
that way rather than inferring it. In the same spirit, `summary` treats the
first round as a baseline rather than a change: Orqyn arrived and found the
repository as it already was, it did not watch it get there.

## PLAN: an objective becomes ordered, durable work

`plan()` is the second step, and it owns the *shape* of the work. Given an
objective and a decomposition the caller supplies as `TaskSpec`s, it validates
the dependency graph — no self-dependencies, no dependencies on tasks outside
the plan, no cycles — computes a deterministic execution order by topological
sort, and persists the plan and every task in one transaction. Activation is
part of that same transaction: the project's previously active plan is marked
superseded, linked both ways, and never deleted, which is what makes "at most
one active plan per project" an invariant rather than a caller's discipline.

PLAN does not decide *what* the tasks should be. The judgment that "add
authentication" decomposes into model → endpoints → middleware → tests is a
reasoning step, and it stays outside the function so the function stays
deterministic and testable. Orqyn's job here is to make a stated decomposition
real, ordered, and durable — not to invent it.

## ASSIGN: a task becomes an agent's responsibility

`assign()` is the third step, and it is the first one that changes something
outside Orqyn's own beliefs: it hands a task to an agent and starts it. Given a
project, a task, and an agent the caller names, it checks that the handoff is
legal and then commits it.

The legality checks all run before any write, so a refused request writes
nothing:

- The task is in the project's **active** plan. A task a superseded plan named
  is history, and handing it out would restart abandoned work.
- The task is still `todo` and every dependency it names is `done` — a `failed`
  or `cancelled` dependency does not unblock it, because handing out work whose
  premise has broken is how silent rework happens.
- No agent already holds it. The store's handoff *would* displace a sitting
  agent; this step refuses instead, because a reassignment should be a
  deliberate act, not a side effect of asking for the same task twice.
- The agent is available and declares every capability the task requires.

Like PLAN, ASSIGN does not decide *which* agent gets *which* task. That is a
scheduling judgment, and it arrives from the caller. `ready_tasks()` is the
read a future scheduler will drive off: the active plan's tasks that can be
handed out right now, in execution order, derived fresh on every call — there
is no stored `ready` flag to fall out of step.

### One transaction, not two

The store's `assign_and_start` does not just insert an assignment. In one
transaction it releases whoever held the task, inserts the new assignment as
active, points the incoming agent's `current_task` at it, moves the task to
`in_progress`, and records the status transition in the history. The
alternative — an assignment followed by a separate status update — leaves a
window in which the task is assigned but still reads `todo`. A crash in that
window is exactly the state the loop cannot recover from cleanly, and nothing
about either row would say the write was interrupted. Folding the two writes
together removes the window, which is why the step never calls `assign_task` on
its own.

## What each step deliberately does not do

No step reaches past its own responsibility. OBSERVE does no planning, no
assignment, no verification — a round changes what Orqyn *knows* and nothing
else, and `ObserveError` is deliberately flat (`Store` versus `Git`, with no
source chains) because a caller that wants to react has to know which of the
two disjoint failures happened. PLAN does not invent tasks; ASSIGN does not
choose agents; MONITOR does not decide what happens to the work it reclaimed.
Each step's error enum is flat for the same reason.

## MONITOR: a lease is only as good as the evidence the holder is still there

`monitor()` is the fourth step, and it is the first one that *revises* a
decision an earlier step made: ASSIGN handed a task to an agent, and MONITOR
takes it back when the agent stops being there to hold it. Every task in
flight is a claim on an agent's attention, so a round looks at each one, asks
what the holder's heartbeats say, and reclaims the leases that have run out.

Liveness is derived, never stored. `Agent::liveness(now)` computes the lease
question from `last_seen` through the same 30/60-minute windows
`status_from_heartbeat` uses — the windows handoff-mcp's `AgentRecord` TTL
design uses, so Orqyn and the substrate agree when both are running. There is
no `alive` flag to fall out of step and no cached verdict to invalidate, which
matters because the one thing a monitor must never do is reclaim a lease from
an agent whose heartbeat it has not checked.

A round reports four things and writes in one of them:

- **`Working`** — the holder's heartbeat is fresh. The lease stands.
- **`Stale`** — the holder has gone quiet, but not past the stale window. The
  lease stands, deliberately: a quiet agent may be thinking, and a lease that
  ends early is work that has to restart. `Stale` is a report, not an
  intervention, and the round writes nothing about it.
- **`Expired`** — the holder is past the stale window. The lease is reclaimed.
- **`Orphaned`** — a task reading `in_progress` with no active assignment
  behind it, the state a stranded handoff would leave behind. The store's
  atomic handoff is what makes this rare; MONITOR reports it and writes
  nothing, because deciding what to do with work no one holds is REPLAN's job.

### One transaction, not five

The store's `expire_lease` does not just release an assignment. In one
transaction it releases the tenure as `LeaseExpired`, closes the session that
held the work as `Vanished`, records the agent as `Disconnected`, clears its
denormalized `current_task` view, and puts the task back to `todo` with a
status transition recording the move. The alternative — a release followed by
separate status and session writes — leaves a window in which the task reads
`in_progress` with no one holding it, which is exactly the orphaned state
MONITOR exists to *report* rather than to create. Folding them together makes
"an expired lease is a `todo` task with a retained tenure" a property of the
write rather than an ordering the caller has to get right.

Two details are in that transaction for specific reasons. The agent is marked
`Disconnected` because without it the loop would hand the vanished agent the
work right back — but an operator's `Offline` is left standing, since that is
a stronger statement than a heartbeat's absence. And the session is closed
*only if it was still live*, so an expiry that races a clean close leaves the
clean close's better evidence in place instead of overwriting it with a
disappearance it inferred.

### Acknowledgment: a handoff gains the invocation doing the work

ASSIGN records a handoff; it does not record that the agent ever began. That
is `acknowledge()`, and it is MONITOR's concern for the same reason the expiry
is: a tenure with a session behind it is what a later lease expiry closes as
`Vanished`, and what makes "which invocation of which agent did this task" an
ordinary query rather than a reconstruction.

The caller names the two ids — the tenure and the invocation — and Orqyn
derives the session's project, task, agent, and machine from the records the
assignment points at, so the session cannot disagree with the tenure it
belongs to. The write is one transaction: the session row is inserted, the
assignment's `session_id` is pointed at it, and the agent is marked `Busy`. A
released tenure cannot acknowledge a session — that would give an ended
assignment evidence of work it never did — and a tenure with a session cannot
acknowledge a second.

### Why a quiet round is not an empty report

A round that found nothing to reclaim returns a report full of `Live` entries
rather than an empty one. The distinction matters to a caller that polls: an
empty report is ambiguous between "nothing is in flight" and "everything in
flight is fine", and a loop that treats those the same will miss the day the
plan finishes. `is_quiet()` is the predicate a polling loop wants, and it is
true only when every task the round surveyed is still being worked.

The round is also idempotent, which is what makes it safe to run on every tick.
A task whose lease expired is `todo`, so the next round does not survey it; a
task whose holder is quiet is reported every round and written never.

Keeping each step that narrow is what makes them testable in isolation: each
one consumes the state the previous one produced and needs nothing more.

## What the crate is not

Not an MCP server, and not a binary. The loop's steps are library functions,
tested directly against real git repositories and a real store without a
process boundary in the way. When the loop is complete enough to run
unattended, a thin binary will wrap it; until then, the tests are the caller.

## What the tests prove

Every step's suite is an integration suite for the same reason: a mock could
only confirm that a step calls the methods it was mocked to expect, and the
risk in this crate is never that. It is that the layers do not line up.

`tests/observe.rs` builds a genuine git repository, commits real content, and
observes it through a real store on disk:

- A first observation registers the repository and records the head commit,
  and reports `first_observation` — including for a repo that already has
  history, which syncs as `Synced` rather than `Baseline`.
- A repeated observation of an unchanged repository reports **no** change —
  which is what a loop polling an idle repo must see, and the thing a
  version-based signal would have gotten wrong.
- A new commit is observed as a change; a dirty working tree is recorded as
  unclean.
- A directory that is not a git repository is refused, and nothing is
  recorded.
- The observation clock is distinguishable across rounds, so observation
  versions advance in order.

`plan.rs` carries unit tests for the half of planning that is pure — request
shape validation and the mapping of graph errors onto plan errors — and
`tests/` covers the half that is not.

`tests/assign.rs` works against a real store and asserts by **reloading** the
records rather than inspecting the value the call returned, because the store
is what the next loop tick reads:

- A ready task is assigned, becomes `in_progress`, and is the active assignment
  for its task; the agent record's `current_task` points at it.
- The handoff and the status move are atomic: an assignment to an agent that
  does not exist fails the insert's foreign key, the transaction rolls back,
  and the task is *not* left `in_progress` with no assignment behind it. A
  rolled-back write records no status transition either.
- A task whose dependency is not done, a task an agent already holds, a busy
  agent, and an agent missing a required capability are each refused — and in
  every case nothing was written: the task is still `todo` and the sitting
  agent still holds it. That is the property that makes retrying cheap.
- A task from a superseded plan is refused, and a project with no active plan
  has nothing to assign at all.
- `ready_tasks` reports exactly the handable work in plan order, excludes
  in-progress and blocked tasks, and recomputes as dependencies complete.

`tests/monitor.rs` works against a real store with the clock pinned, because
the difference between `Working`, `Stale`, and `Gone` is thirty and sixty
minutes and the tests move the heartbeats rather than the clock:

- A fresh heartbeat leaves the lease standing and the round quiet; a stale one
  is reported while the task is still `in_progress` and the agent's own status
  is untouched — a quiet agent is not a dead agent.
- A heartbeat past the stale window loses the lease: the task is back to
  `todo`, the tenure is retained as `lease_expired` rather than deleted, the
  agent holds nothing and is disconnected, and the reclaimed task is handable
  again on the next `ready_tasks` read — the cross-step property the expiry
  exists for.
- A second round after an expiry surveys nothing, which is what makes the step
  safe to run on every tick.
- A task reading `in_progress` with no assignment behind it is reported
  orphaned and left exactly as it was.
- Acknowledgment records a session that agrees with the tenure it belongs to
  and marks the agent busy; the two halves of the step compose, so a session
  the agent acknowledged is closed as `vanished` when its lease runs out.
- Acknowledging an unknown assignment, a tenure twice, or a tenure whose lease
  already expired is refused in each case with nothing written.
