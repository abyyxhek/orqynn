# Phase 6 — The Control Loop, and Its First Step

> **Status: underway.** The `director-app` crate exists and its first step,
> OBSERVE, is landed and tested. The remaining steps are built next, one at a
> time, in the order they run.

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

## What it deliberately does not do

No planning, no assignment, no verification. A round of OBSERVE changes what
Orqyn *knows* and nothing else, and `ObserveError` is deliberately flat —
`Store` versus `Git`, with no source chains — because a caller that wants to
react has to know which of the two disjoint failures happened.

Keeping the step that narrow is what makes the later steps testable in
isolation: each one consumes the state this produces and needs nothing more.

## What the crate is not

Not an MCP server, and not a binary. The loop's steps are library functions,
tested directly against real git repositories and a real store without a
process boundary in the way. When the loop is complete enough to run
unattended, a thin binary will wrap it; until then, the tests are the caller.

## What the tests prove

`tests/observe.rs` are integration tests for a specific reason: every layer
underneath has its own suite, but until this crate nothing composed them. A
unit test over a mock could only ever confirm that `observe` calls the methods
it was mocked to expect; it cannot show that the git service's snapshot shape
and the store's `StoredProjectState` actually line up, which is the whole risk
of the crate. So each test builds a genuine git repository, commits real
content, and observes it through a real store on disk:

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
