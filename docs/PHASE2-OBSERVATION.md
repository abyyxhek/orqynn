# Phase 2 — Git Observation: Director's Own Source of Truth

> Director Brain · Phase 2 deliverable (observation half)
> Date: 2026-09-25
> Scope: `director-domain::repository`, `director-adapters::git`. No MCP server,
> no substrate connection, no loop.

## Goal

Give Director an **independent** way to learn what happened in a working tree —
one that does not depend on any agent reporting correctly, and does not depend
on either substrate.

Phase 1's second invariant is that nothing self-reports completion: `TaskStatus::Done`
is unreachable from an agent's report, and only Director's own verification
engine can set it. That invariant is a promise about the model until Director
has eyes of its own. Phase 2's observation half builds those eyes: Director
reads git itself, records what it saw, and can later compare a claim against
the repository rather than against another report.

This half covers observation only. Deciding what a change *means* for a task —
whether it satisfies a criterion, whether a replan is needed, whether an agent
deserves credit — is the verification engine (Phase 10) and consumes these
types; it does not live here.

## The architectural rule: three jobs, three types

Observing git, recording the observation, and deciding what to do about it are
three different jobs with different failure modes and different test shapes.
Phase 2 separates them:

| Type | Question it answers | Persists? |
|---|---|---|
| `GitObserver` | What happened in git? | No |
| `RepositoryStore` | What has Director recorded? | Yes (JSON) |
| `GitService` | What does this mean for project state? | Yes, via the store |

`GitObserver` is stateless — every call opens the repository afresh, which is
what makes two observations independent of each other. `GitService` is cheaply
cloneable: the store sits behind an `Arc`, so two agent sessions can each hold
a service and still serialize on one lock.

What is deliberately **not** in this layer: anything that decides which agent
should act, how a task should be replanned, or whether a change is semantically
important.

## The decision worth writing down: git is a tool, not a substrate

Phase 0's boundary says Director talks to handoff-mcp and ai-memory
*exclusively over MCP*, and Phase 1's boundary test fails the build if anything
outside `director-adapters` names either one. The observation layer depends on
`git2`, which is neither substrate, and the test permits it.

The distinction is not "git2 is fine because it is not in the list". It is:

- A **substrate** is an external system with its own data model that
  competes with Director's. Coupling to it means inheriting its semantics —
  handoff-mcp's self-reported `done` is the whole reason Phase 1 exists. So
  Director speaks to substrates over a process boundary and translates through
  the wire/mapping layers.
- **git** is a tool Director reads, like `std::process`. Linking a git library
  imports a data format, not a competing model of tasks or agents. The same
  reasoning covers `LocalExecutor`: running a command is tool use, and Director
  reads the exit code itself.

The test's rule is therefore *no coupling to a competing model*, not *no
dependencies at all*. Git is also the substrate-independent ground truth: it is
the one thing Director can inspect that neither substrate controls.

## What was built

### Domain types — `director-domain::repository`

The adapter owns the git2 handle; the domain owns the shape of the answer.

- **`Repository`** — a *record of what was seen*, not a live handle to git.
  Carries the canonical path, remote URL, default branch (never assumed to be
  `main` — a repository on `develop` or `trunk` reports its own), checked-out
  branch (`None` under detached HEAD), current and last-observed commit, and a
  monotonic observation version.
- **`WorktreeState`** — git's flat `(xy-code, path)` status report bucketed
  into the categories Director reasons about: modified, added, deleted,
  renamed, copied, untracked. Nothing is lost in normalization — every entry
  keeps its git-reported kind, and renames keep both paths. `clean` is
  **derived** from the buckets rather than passed in, so it can never disagree
  with them.
- **`ProjectStateSnapshot`** — one repository's state at one moment, with the
  comparison logic: `state_changes` yields `BranchChanged` / `CommitChanged` /
  `WorktreeChanged`, and `has_important_change` is what a later loop will key
  on. Distinct from `state::StateComparison`, which compares a *checkpoint*
  against observed state to decide whether a resume is safe.
- **`ObservationEvent`** — one fact Director learned, with a typed `EventData`
  payload (kept structured rather than a free `Value` so consumers match on it
  without re-parsing) and a `source` field. Git is the only source this phase;
  filesystem and verification sources can join later without changing the
  shape.
- **`EventKind`** — eight variants, plus `for_file_change`, the mapping from a
  working-tree change to an event. Two choices there are load-bearing: an
  untracked file maps to `FileAdded` (from the project's point of view it has
  still been added), and a conflicted file maps to `FileModified` (a tracked
  file whose content is in dispute — never a silent no-event).
- **`SyncResult` / `SyncStatus`** — the structured answer to "what happened
  when I synced?". `SyncStatus` distinguishes `Synced` (new facts recorded),
  `NoChange` (nothing moved), and `Baseline` (first observation — a baseline
  was established rather than a change detected). First observation is not
  reported as a giant change, because it isn't one.
- **`RepositoryError`** — path handling is strict because repository paths are
  untrusted input. A non-absolute path, or one that escapes through a `..`
  component, is rejected rather than normalized away. `NotARepository` is an
  error, never silently accepted as a status.

### The observer — `director-adapters/src/git/observer.rs`

A read-only lens: branch, HEAD commit, worktree status with rename and copy
detection, remote URL, default branch, recent commits, commit-range walk, diff
contents. `open()` validates the path and then checks the opened repository's
working directory **back** against the requested path, because
`Repository::open` searches upward — a caller pointing at a subdirectory must
not have Director silently observe a repository rooted above it.

Two consequences of the implementation choice:

- **No injection surface.** git2 is in-process; there is no shell in the loop.
  The observer accepts a *path*, never a command, and exposes only named
  operations. There is no method on the type through which a caller could get
  arbitrary text executed, because it never builds a command at all. Using a
  library rather than parsing `git` output also removes the fragile
  shell-string parsing class of bug.
- **Bounded walks.** `MAX_WALK = 200` commits per incremental walk, so a
  force-push or a first observation of a long history does bounded work rather
  than walking a million commits. When the anchor commit the previous snapshot
  was built on is missing from current history, `RangeWalk` reports
  `history_rewritten: true` and falls back to a bounded recent window rather
  than failing — a rebase, amend, or force-push is a normal git occurrence, not
  an error.

### The store — `director-adapters/src/git/store.rs`

One JSON file per repository, written atomically, with optimistic version
checks and a lock the service composes through. Holds the last observed state
so the next observation compares against something rather than nothing.
`append_events` / `append_commits` / `append_file_changes` each return how many
items were genuinely new — idempotency is observable, not just claimed.

### The service — `director-adapters/src/git/service.rs`

Composes the two into `GitService`: `initialize_repository` establishes the
baseline (recording the 50 most recent commits as context, not as change
events), and the incremental sync compares, derives events, and persists.
`observe_worktree` reads live state without persisting, for callers that need
"what is the tree like right now".

## Idempotency: the same observation is one event, not two

This is the property the whole layer exists to have, and it is enforced in
three places:

1. **Change identity excludes the change kind.** A file that is added and then
   edited is the same path at two different contents — one addition and one
   modification. If the kind participated in identity, the second observation
   would look like a brand-new change of a different shape. With the fingerprint
   alone — path, previous path, anchor commit, content hash — both facts are
   recorded and each is correct.
2. **A repeated dirty path is not re-reported.** The same uncommitted edit
   observed twice is one event, unless the observation anchor moved.
3. **A path that goes dirty → clean is not a deletion.** That is the ordinary
   signature of a commit, not a removal; reporting it as `FILE_DELETED` would
   be wrong.

`SyncResult::events_created` is `0` for a no-op re-sync. A caller can verify
idempotency from outside.

Deletions have no content to hash, so they fingerprint as a constant distinct
from any real blob oid; a file present but with a zero object id fingerprints
as another distinct constant, so it can never collide with a real blob either.

## One sync, step by step

1. `open()` validates and canonicalizes the path, then confirms the opened
   repository is rooted exactly there.
2. Read branch, HEAD commit, worktree state, and — for a repository Director
   has seen before — the previous snapshot from the store.
3. Walk the commit range from the previous anchor to HEAD, oldest first (the
   order they happened, which is the order events are emitted in). If the
   anchor is missing, report `history_rewritten` and fall back to a bounded
   window.
4. Derive events: commits from the walk, file events from
   `WorktreeState::new_changes`, a branch event if the branch moved.
5. Append to the store under its lock; the store reports how many were
   genuinely new.
6. Write the new snapshot and update the repository record; return a `SyncResult`.

## `CommitInfo` was widened

`state::CommitInfo` carried only a summary. Phase 2 widened it to the full
canonical commit model — `parents` (empty for a root commit, two for a merge),
`committer`, and the complete `message` body, with `#[serde(default)]` so
payloads written by Phase 1 still deserialize. The fields carry the comments
explaining why each defaults.

The widening carries one rule with it: **a commit message is evidence, not
verified truth.** Director records it; no task is ever completed because a
message says "done". This is the same invariant as `Done` being unreachable by
agent report, extended to the repository — a commit titled `fix: done` is a
fact Director observed, not a conclusion it reached.

## Definition of done — status

- [x] `director-domain::repository`: `Repository`, `WorktreeState`,
      `ProjectStateSnapshot`, `ObservationEvent`, `EventKind`, `EventData`,
      `SyncResult`, `SyncStatus`, `RepositoryError`, plus `RepositoryId` and
      `EventId` in `ids`.
- [x] `CommitInfo` widened with serde defaults for backwards compatibility.
- [x] `FileChange::Copied` added, with `has_old_path` / `is_addition` helpers.
- [x] Observer: read-only git2 lens, path validation, bounded walks, history
      rewrite detection.
- [x] Store: atomic JSON persistence, optimistic version checks, lock.
- [x] Service: baseline initialization, incremental sync, event derivation,
      idempotency.
- [x] `git2 = "0.21"` in `director-adapters`; `sha2` workspace dep.
- [x] All four gates clean: `cargo test --all` (174 tests), `cargo fmt --check`,
      `cargo clippy --all-targets -- -D warnings`.
- [x] Boundary test still passes: the observation layer is tool integration
      inside `director-adapters`, not substrate coupling.

## Test coverage notes — including the gaps

The 42 adapter unit tests break down as 10 in `store.rs`, 1 in `service.rs`,
and the pre-existing handoff/memory/executor tests. The store is well covered:
atomic writes, version checks, event idempotency.

Two gaps are worth naming rather than papering over:

1. **`observer.rs` has no tests of its own.** It is the layer that actually
   touches git2, and it is exercised only transitively, if at all. Its path
   validation rules — the `..` rejection and the rooted-elsewhere check — are
   security-relevant and deserve direct tests.
2. **`service.rs` claims behaviour is "exercised in `tests/git_integration.rs`",
   and that file does not exist.** `crates/director-adapters/tests/` holds only
   `boundary.rs`. The comment overstates coverage that has not been written.

Both point at the same missing artifact: an integration test suite that builds
real temporary git repositories (the `tempfile` dev-dependency is already
declared for exactly this) and drives the observer and service end to end —
baseline, no-op re-sync, new commit, branch move, rebase. That suite would
close both gaps at once and is the natural first task of whatever phase picks
the observation layer back up.

## What this half deliberately does not do

- **No connection to either substrate.** The handoff-mcp wire types, transport,
  and mapping exist, but `HandoffAdapter` does not. Nothing composes them into
  provider traits yet.
- **No semantic interpretation.** A commit is a commit; whether it satisfies an
  acceptance criterion is a later question.
- **No writes to git, ever.** Director does not commit, push, branch, or merge
  on an observed repository. Read-only is a property of the type: there is no
  method to call.
- **No persistence of Director's own state.** The JSON store holds *observed
  repository state* — a cache for change detection. Checkpoints, plans,
  decisions, and verifications remain Phase 5.
- **No event retention policy.** Events accumulate without bound. Pruning by
  age or count is a later concern.
