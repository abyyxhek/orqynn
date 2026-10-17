# Orqynn (Director Brain) — Next Step Audit: Repository Verification, Stabilization & CI Readiness

**Audit date:** 2026-10-16
**Auditor:** opencode (source-and-history-driven audit, gates executed)
**Scope:** repository verification, implementation status, validation baseline, Phase 13 audit, CI readiness, next-milestone recommendation.
**Explicitly out of scope for this audit (per instruction):** Phase 8 (Multi-Identity Agent Pool) and Phase 9 (RecentContext Engine) were **not** implemented. They are assessed and recommended only.

> **Naming note.** This report covers **Orqynn** only. It is unrelated to the pre-rebrand working name "Orqyn" and entirely unrelated to **SNAGLINE** (a separate project located at `C:\Users\ASUS\Desktop\abishek\snagline`, confirmed absent from this repository and its history).

---

## 1. Repository Identity and HEAD

### 1.1 Authoritative repository — confirmed

| Property | Value | Verification |
|---|---|---|
| Working directory | `C:\Users\ASUS\OneDrive\Desktop\ai_memory\orqyn` | `Get-Location` |
| Git repository root | `C:/Users/ASUS/OneDrive/Desktop/ai_memory/orqyn` | `git rev-parse --show-toplevel` |
| Remote (fetch & push) | `https://github.com/abyyxhek/orqynn.git` | `git remote -v` |
| Branch | `main` | `git rev-parse --abbrev-ref HEAD` |
| HEAD | `65cfa02eb534d7e77533891a1d9f3a882d1d77ea` | `git rev-parse HEAD` |
| HEAD subject | *docs: sync README with the real API; pin the example with a compiled test* | `git log -1` |
| HEAD date / author | 2026-10-14 13:10:47 +0530 / `Abishek` | `git log -1 --format` |
| Relationship to `origin/main` | **0 ahead / 0 behind** | `git rev-list --left-right --count HEAD...origin/main` |
| Tracked working-tree modifications | **NONE** | `git status --porcelain` filtered to non-`??` |
| Untracked files | **44** (42 pre-existing scratch/report files + this audit's 2 new files) | `git status --porcelain` |

✅ **This is the authoritative `abyyxhek/orqynn` repository.** Confirmed by three independent signals: the git remote, `Cargo.toml:15` (`repository = "https://github.com/abyyxhek/orqynn"`), and `README.md:468`.

⚠️ **The local directory is named `orqyn`, not `orqynn`.** This is a filesystem artefact only — the remote, manifest and README are all correct. It is a persistent confusion risk and is the reason this section exists.

### 1.2 Alternate checkouts inspected (read-only, nothing modified)

| Directory | Git root | Remote | HEAD | Status |
|---|---|---|---|---|
| **`orqyn/`** ← authoritative | `.../ai_memory/orqyn` | `abyyxhek/orqynn` | `65cfa02` | clean, in sync |
| `director-brain/` | `.../ai_memory/director-brain` | `abyyxhek/orqynn` | `37f39b8` | **ahead 1 / behind 2**, 6 modified + 1 untracked |
| `ai-memory/` | `.../ai_memory/ai-memory` | `akitaonrails/ai-memory` | `24ae5dd0b` | behind 3309 — **substrate, not this project** |
| `handoff-mcp/` | `.../ai_memory/handoff-mcp` | `alphaelements/handoff-mcp` | `e367255` | behind 86 — **substrate, not this project** |
| `orqyn-doc-backup-20261012-201821/` | — | — | — | not a git repo (backup dir) |
| `orqyn-reconciliation-backup-20261013-214803/` | — | — | — | not a git repo (backup dir) |
| `~/snagline-pr`, `~/Desktop/abishek/snagline` | — | — | — | **SNAGLINE — unrelated project, no relationship to Orqynn** |

`git worktree list` reports a **single** worktree (`orqyn/`). `director-brain/` is therefore an independent clone, not a linked worktree — so the two share no index or ref state, which makes accidental cross-contamination easy.

### 1.3 Refs and stashes (all inspected, none touched)

```
refs/heads/main                     65cfa02
refs/heads/backup/pre-scrub-main    84134f8  "README: record Phase 2 as complete"
refs/original/refs/heads/main       84134f8  (filter-branch backup ref)
refs/remotes/origin/main            65cfa02
refs/stash                          1de6d53  "WIP on main: 65cfa02"
```

`git stash list` → **1 stash**: `stash@{0}: WIP on main: 65cfa02`. This is the pre-Phase-12 snapshot of the same automatic-checkpoint work that is now finished and committed at `bda3382`. It is **superseded** — it should be retained until the author confirms, not applied.

`refs/heads/backup/pre-scrub-main` (84134f8) holds a duplicate Phase-2-era commit series that pre-dates the cherry-pick onto the rebrand line. Harmless, but it is the source of the "no merge base" reconciliation both prior audits describe.

### 1.4 The `director-brain/` divergence — inspected, not modified

```
## main...origin/main [ahead 1, behind 2]
 M crates/director-app/src/lib.rs           |   7 ++
 M crates/director-app/src/observe.rs       |  58 ++++++-
 M crates/director-domain/src/checkpoint.rs |  13 ++-
 M crates/director-domain/src/store.rs      |  22 +++-
 M crates/director-store/src/checkpoints.rs |  65 ++++++--
 M crates/director-store/tests/store.rs     | 173 ++++++++++++++++++++++++++++++-
 6 files changed, 318 insertions(+), 20 deletions(-)
?? crates/director-app/src/checkpoint.rs        (636 lines)
```

**What it is:** the Phase 12 automatic-checkpoint writer at an **earlier iteration** than the version already committed on `main` (`bda3382`). Its untracked `checkpoint.rs` exposes `auto_checkpoint(...)` rather than `checkpoint_round(...)`, mints ids from `&mut IdGenerator`, and lacks `should_checkpoint`, `restates_current`, `CheckpointConflict` and lineage-derived `CHK-<task>-N` ids — i.e. it is precisely the code that re-minted `CHK-work-1` and aborted an observation round on a primary-key collision, which `docs/PHASE12-CHECKPOINT-RECOVERY.md` documents as the defect that Phase 12 fixed.

**The specific hazard:** its diff **removes `self.state_version += 1` from `Checkpoint::supersede()`**, whereas committed `main` **keeps** the increment. Applying this work onto `main` would be a **silent data-model regression**, not a merge conflict — it would invert the meaning of `state_version` on superseded checkpoint rows. It would also revert the README from "Phases 1–3, 5–7, 10–12 Complete / 608 Passed" to "10–11 Complete / 538 Passed".

**Recommendation:** treat `director-brain/` as read-only archaeology. **Do not merge, reset, clean, rebase or cherry-pick it.** Confirm nothing unique survives, then retire the clone. This is the author's decision; this audit did not touch it.

### 1.5 Preservation statement

No destructive Git command was run. Nothing was reset, cleaned, stashed, reverted, committed or pushed. **No tracked file was modified** (`git diff --stat HEAD` is empty). Two files were added, both new and both explicitly requested: `.github/workflows/ci.yml` (Step 5, authorized) and `ORQYNN_NEXT_STEP_AUDIT.md` (this deliverable).

---

## 2. Verified Architecture and Implementation Status

Every item below was checked **directly against source**, not taken from prior audit documents.

### 2.1 The four-crate architecture — ✅ Verified complete

Confirmed from `Cargo.toml:1–8` and every crate manifest:

| Crate | Version | Key dependency | Lint posture |
|---|---|---|---|
| `director-domain` | 0.1.0 | *(none external beyond serde/chrono/thiserror/async-trait/sha2)* | `#![warn(missing_docs)]`, `#![forbid(unsafe_code)]` (`lib.rs:47–48`) |
| `director-adapters` | 0.1.0 | `git2 = "0.21"` | `#![warn(missing_docs)]`, `#![forbid(unsafe_code)]` (`lib.rs:34–35`) |
| `director-store` | 0.1.0 | `rusqlite = "0.32"` (`bundled`, `chrono`, `serde_json`) | `#![allow(dead_code)]` at `lib.rs:59` — **no `forbid(unsafe_code)`** |
| `director-app` | 0.1.0 | `tokio`, `async-trait`, `anyhow`, `tracing` | `[lints.rust] unsafe_code = "forbid"` |

Dependency direction is strictly inward and was **not** violated: `app → store → domain`, `app → adapters → domain`. `adapters` does **not** depend on `store`; `store` does **not** depend on `adapters`. Module inventories: domain 21 modules, adapters 7 module roots, store 14 modules, app 10 modules — **63 source files**, 26,298 src LOC (~20,341 production, excluding `#[cfg(test)]` blocks).

### 2.2 Orchestration flow `OBSERVE → PLAN → SCHEDULE → ASSIGN → MONITOR → VERIFY → REPLAN` — ⚠️ Partially implemented

| Step | Entry point (verified signature) | Status |
|---|---|---|
| OBSERVE | `observe::observe(&Store, &GitService, &mut IdGenerator, &impl MemoryProvider, &ProjectId, &RepositoryId, &str) -> Result<Observed, ObserveError>` (`observe.rs:141`) | ✅ Verified complete |
| CHECKPOINT | `checkpoint::checkpoint_round(...) -> Result<CheckpointRound, CheckpointError>` (`checkpoint.rs:436`), runs **inside** OBSERVE | ✅ Verified complete |
| PLAN | `plan::plan(&Store, PlanRequest) -> Result<Planned, PlanError>` (`plan.rs:188`) | ✅ Verified complete **as validation+ordering**; ⚠️ does not decompose |
| SCHEDULE | `schedule::schedule(&Store, &mut IdGenerator, &ProjectId) -> Result<ScheduleReport, ScheduleError>` (`schedule.rs:130`) | ✅ Verified complete; ❌ **no scope-conflict awareness** (§5) |
| ASSIGN | `assign::assign(&Store, AssignRequest) -> Result<Assigned, AssignError>` (`assign.rs:107`) | ✅ Verified complete; ❌ **no scope-conflict awareness** (§5) |
| MONITOR | `monitor::monitor(&Store, &ProjectId) -> Result<MonitorReport, MonitorError>` (`monitor.rs:169`) + `acknowledge`, `report_done` | ✅ Verified complete |
| VERIFY | `verify::verify(&Store, &impl ExecutionProvider, &ProjectId, &Path)` (`verify.rs:266`) and `engine::verify_task(...)` (`engine.rs:115`) | ✅ Verified complete **for `Probe::Command` only** |
| REPLAN | `replan::replan(&Store, ReplanRequest) -> Result<ReplanReport, ReplanError>` (`replan.rs:379`) | ✅ Verified complete **given a caller-supplied remediation** |

**⚠️ The loop is not driven.** Grep for `run_loop`, `daemon`, `#[tokio::main]`, `fn main`, `clap`, `interval(` across all `src/` returns **zero** production hits. The only `loop {}` statements are a `try_wait` poll in `executor.rs:86` and a JSON-RPC read loop in `stdio_mcp.rs:232`.

**Authoritative confirmation via `cargo metadata --no-deps`** (cannot be faked by grep):

```
director-domain:   targets = lib:director_domain
director-adapters: targets = lib:director_adapters, test:aimemory_live, test:boundary,
                            test:handoff_live, test:wiring
director-store:    targets = lib:director_store, test:store
director-app:      targets = lib:director_app, test:assign, test:checkpoint_lineage,
                            test:end_to_end, test:engine, test:monitor, test:observe,
                            test:readme_example, test:recovery_builder, test:replan,
                            test:runtime_recovery, test:schedule, test:verify
```

**Every target is a `lib` or a `test`. There is no `bin` target anywhere in the workspace.**

### 2.3 SQLite persistence, migrations, concurrency, versioning, checkpoint lineage, recovery — ✅ Verified complete (with noted gaps)

**Persistence.** 3 migrations (`0001_initial.sql` 258 lines, `0002_plans_decisions.sql` 57, `0003_verifications.sql` 36), **15 tables** + `schema_migrations`, **26 index statements**.

**Migrations** (`migrations.rs:63–235`): embedded via `include_str!`, so a listed-but-missing file **fails the build**. Versions must be contiguous (asserted by test). The runner **refuses to open a database newer than the build** (`migrations.rs:141–149`) rather than corrupting it. Verified: `opening_creates_the_schema_and_is_idempotent` passes.

**Concurrency protection** (`connection.rs:169–238`): `journal_mode=WAL` is **read back and rejected if it is not `wal`**; `foreign_keys=ON`; `busy_timeout=5000 ms`; pool size 8. Optimistic concurrency via `state_version INTEGER NOT NULL DEFAULT 1` on every mutable table, written as `UPDATE … SET state_version = state_version + 1 WHERE id = ? AND state_version = ?`, with zero affected rows converted to `StoreError::StateVersionConflict` at `row_count_to_outcome()` (`connection.rs:224`). **Verified by test:** `two_concurrent_writers_on_the_same_version_see_one_winner_and_one_conflict` — passes.

**Task versioning.** `Task.state_version` (`task.rs:215`), baseline 1 not 0 (`task.rs:222–228`), bumped only by the store on successful update (`task.rs:317–320`). **Verified by test:** `cancel_task_on_a_stale_read_is_a_conflict_not_an_overwrite` — passes.

**Checkpoint lineage.** `CheckpointStatus::{Current, Superseded}`; `create_checkpoint(&self, checkpoint, expected_current_version: Option<u64>)` is the optimistic guard (`domain/store.rs`), where `None` asserts *no* current checkpoint exists. Ids are **lineage-derived** (`CHK-<task>-N`), not process-local counters — the fix for a real primary-key collision. Per-task conflicts are **collected** into `CheckpointRound.conflicts` with `retry_against()` so one stale writer does not lose every other task's checkpoint. **Verified by test:** `checkpoint_lineage` suite passes; `the_first_checkpoint_for_a_task_is_created_against_a_none_precondition`, `expecting_a_version_for_a_task_with_no_checkpoint_conflicts` pass.

**Recovery context.** `RecoveryBuilder` (`recovery.rs:313`) assembles `ContinuationPackage` (15 fields) — task contract, version history, checkpoint lineage, prior verdicts, git comparison — and SCHEDULE attaches it to `Scheduled.recovery` before the write. **Verified by test:** `tests/recovery_builder.rs`, 18 tests — all pass.

**⚠️ Gap:** `RecoveryGaps::structural()` (`recovery.rs:160`) hardcodes `recent_context_unavailable: true, blockers_unavailable: true`. Its own comment concedes: *"False for every build today, because both gaps are structural."* So `Recovery::is_clean()` is always `false` and `actions_available()` is always `false`.

### 2.4 MCP client adapters and Git integration — ✅ Verified complete (client side only)

- `StdioMcpTransport` (`stdio_mcp.rs`): `tokio::process::Command`, `env_remove` applied **before** `extra_env` (documented ordering), stderr **nulled**, `initialize` → `notifications/initialized` handshake, `Drop` for shutdown, and a **redacted `Debug`** exposing only `next_id`.
- `HandoffAdapter<T: HandoffWire>` and `AiMemoryAdapter<T: AiMemoryWire>` each have **three** wire implementors: the real transport, `Mutex<Fake…>` and `Mutex<Broken>` — which is what makes the substrate testable.
- Git: `GitObserver` (read-only `git2` lens), `GitService`, `RepositoryStore` (JSON + file lock + optimistic version check), and `director-domain::repository` (1,261 lines).
- ⚠️ **Limitation (documented, not fixed):** the MCP `extra` channel is closed in **both** directions, so an Orqynn-only status and a `Critical` priority do **not** survive a substrate round trip.

### 2.5 Substrate boundary and its enforcement tests — ✅ Verified complete

- `crates/director-adapters/tests/boundary.rs` — scans the workspace and **fails the build** if any crate outside `director-adapters` names a substrate identifier or depends on one. 1 test, passing.
- `crates/director-adapters/tests/wiring.rs` — fails the build if a source file is not declared in the module tree. 3 tests, passing. Added in commit `cf2231e` specifically after the `plan.rs` "610 lines never compiled" incident recorded in `AUDIT-2026-09-30.md`.

Both are **verified passing**, and both are the kind of test that has already prevented a real defect. This is a genuine strength.

### 2.6 Executable entry point, CLI, daemon, Orqynn MCP server — ❌ Missing

Established authoritatively via `cargo metadata` (§2.2): **zero `bin` targets**. There is no `[[bin]]` in any manifest, no `src/main.rs`, no `examples/`, no `benches/`, no CLI argument parsing (`clap`/`structopt` absent from every dependency), and no `#[tokio::main]`.

**Orqynn is an MCP *client* only. It serves no tools of its own.** Orqynn's own MCP server does not exist.

`director-app/src/lib.rs:39–44` states this plainly: *"Not an MCP server, and not a binary yet … what is missing is a driver. Until one exists, the tests are the caller."*

### 2.7 `Task::scope_paths` / `Task::overlaps_scope` in production scheduling & assignment — ❌ Missing

Full analysis in **§5**. Summary: the field is persisted, read back, and round-tripped through the MCP wire, and the predicate exists and is unit-tested — but it has **zero production callers**, and neither `schedule.rs` nor `assign.rs` contains any scope, conflict or overlap logic whatsoever.

### 2.8 Can agent-reported success mark a task complete without verification? — ✅ Verified complete: **No.**

Verified at four independent layers:

1. **State machine.** `TaskStatus::reachable_by_agent_report()` (`task.rs:149–155`) returns exactly `{InProgress, Blocked, VerificationPending}`. `Done` is excluded. Test `!TaskStatus::Done.reachable_by_agent_report()` (`task.rs:360`) passes.
2. **Store write path.** A workspace-wide grep for `TaskStatus::Done` across all `src/` returns **two production write sites**: `verify.rs:444` (`Verdict::Passed => Some(TaskStatus::Done)`) and `domain/verification.rs:235` (`VerificationStatus::Passed => Some(TaskStatus::Done)`). Every other occurrence is a `#[cfg(test)]` line, a doc comment, or a `== Done` comparison. The store writes `VerificationPending` on agent completion (`store/src/assignments.rs:770, 836`); `store/src/verifications.rs:349` sets `VerificationPending` as a *rollback*, not a completion.
3. **Provider traits.** None of the 7 traits in `providers.rs` can complete a task.
4. **Substrate boundary.** `handoff/mapping.rs:81` `task_from_wire(data, trusted_done_ids)` takes an explicit externally-trusted done set, so a substrate self-report of `done` is not believed by default.

**Verified by test:** `a_failing_verification_moves_the_task_to_failed_and_not_done`, `unverifiable_evidence_does_not_mark_the_task_failed`, `report_completion_awaits_verification_without_completing` — all pass.

⚠️ One low-severity observation: `handoff/mapping.rs:265` and `:301` map **any unrecognized** substrate status string to `VerificationPending` via a `_ =>` fallback rather than raising an error. This fails **safe** (it cannot reach `Done`), but an unknown status is silently reinterpreted rather than surfaced.

### 2.9 Timeouts, dirty-worktree deduplication, checkpoint recovery gaps

| Concern | Classification | Evidence |
|---|---|---|
| **Command timeout** | ✅ **Verified complete** | `executor.rs:24` `DEFAULT_TIMEOUT = 60s`; `with_timeout()` at `:56`; `CommandOutcome.timed_out` set correctly (`:101`, `:120`); tested at `executor.rs:202–223`. `CommandOutcome::succeeded()` = `exit_code == 0 && !timed_out`, so a timeout is never mistaken for success |
| **Async blocking** | ⚠️ **Partially implemented** | `LocalExecutor` polls synchronously and is **not** wrapped in `spawn_blocking`. `executor.rs:13` contains the doc comment *"Phase 10 wraps this in `spawn_blocking`"* — but a workspace-wide grep finds `spawn_blocking` **only** in that comment. **Phase 10 is complete and this remediation was never applied.** Harmless today (no daemon, tests are serial); a real hazard once a daemon runs VERIFY concurrently |
| **Lease TTL / stale reclamation** | ✅ **Verified complete** | `monitor::monitor` compares heartbeat age against TTL, expires the lease, marks the session `vanished`, records the disconnect, returns the task to `todo`. Verified by 19 passing monitor tests |
| **Dirty-worktree deduplication** | ✅ **Verified complete** — genuinely well designed | `should_checkpoint` (`checkpoint.rs:211`) requires `TaskStatus::InProgress` **and** `SyncStatus::Synced`. `restates_current` (`checkpoint.rs:255`) suppresses a checkpoint whose evidence duplicates the current one. Its rationale documents the precise hazard: for untracked/unhashed files the git layer's content key falls back to `len:mtime` in **whole seconds**, so a file rewritten with identical bytes is a "new change" — and without this guard the lineage would grow unboundedly with identical entries. Both a quiet round and a suppressed round write nothing, and neither is an error |
| **Checkpoint recovery gaps** | ⚠️ **Partially implemented** | `RecoveryGaps::structural()` hardcodes both flags `true` (§2.3). `Blocker` has **no table, no migration, no repository** and is never raised. `RecentContext` is in-memory, unpersisted and never called by the loop |

---

## 3. Validation Commands and Results

All commands executed **in this audit**, from the verified repository root `C:\Users\ASUS\OneDrive\Desktop\ai_memory\orqyn`, on 2026-10-16 at `65cfa02`, on `rustc 1.98.1 (48a229cea 2026-09-01)` / `cargo 1.98.1`.

| # | Command | Exit | Result |
|---:|---|---:|---|
| 1 | `cargo fmt --all --check` | **0** | Clean — no output, no diff |
| 2 | `cargo test --workspace --all-targets` | **0** | **606 passed, 0 failed, 13 ignored** (21 test binaries) |
| 3 | `cargo test --workspace --doc` | **0** | **2 passed, 0 failed** — `director_domain` `ids.rs:244` (run) + `director_store` `lib.rs:30` (compile-only). `director_adapters` and `director_app` have 0 doctests each |
| 4 | `cargo clippy --workspace --all-targets -- -D warnings` | **0** | Clean — **zero warnings** under `-D warnings` |
| 5 | `cargo build --workspace` | **0** | Finished, no errors, no warnings |

**Total: 608 passing, 0 failing.** (606 unit/integration + 2 doctests.) This exactly matches the README badge at `README.md:10` — **the previously reported figure was correct.**

### 3.1 Per-suite breakdown (executed)

| Suite | Target | Passed | Failed | Ignored |
|---|---|---:|---:|---:|
| director-adapters unit | `--lib` | 77 | 0 | 0 |
| director-domain unit | `--lib` | 168 | 0 | 0 |
| director-store unit | `--lib` | 29 | 0 | 0 |
| director-store integration | `test:store` | 69 | 0 | 0 |
| director-app inline unit | `--lib` | 134 | 0 | 0 |
| app assign | `test:assign` | 15 | 0 | 0 |
| app replan | `test:replan` | 21 | 0 | 0 |
| app monitor | `test:monitor` | 19 | 0 | 0 |
| app observe | `test:observe` | 13 | 0 | 0 |
| app schedule | `test:schedule` | 13 | 0 | 0 |
| app verify | `test:verify` | 13 | 0 | 0 |
| app runtime_recovery (Phase 11 proofs) | `test:runtime_recovery` | 5 | 0 | 0 |
| app recovery_builder (Phase 12) | `test:recovery_builder` | 18 | 0 | 0 |
| app checkpoint_lineage (Phase 12 regression) | `test:checkpoint_lineage` | 1 | 0 | 0 |
| app end_to_end | `test:end_to_end` | 2 | 0 | 0 |
| app readme_example | `test:readme_example` | 1 | 0 | 0 |
| app engine | `test:engine` | 6 | 0 | 0 |
| adapters wiring | `test:wiring` | 3 | 0 | 0 |
| adapters boundary | `test:boundary` | 1 | 0 | 0 |
| adapters ai-memory **live** | `test:aimemory_live` | 0 | 0 | **8** |
| adapters handoff **live** | `test:handoff_live` | 0 | 0 | **5** |
| doctests (2 crates) | `--doc` | 2 | 0 | 0 |
| **TOTAL** | | **608** | **0** | **13** |

### 3.2 Test classification and environmental prerequisites

| Class | Count | Prerequisite | Status here |
|---|---:|---|---|
| **Unit tests** (inline `#[cfg(test)]`) | 408 | none beyond a Rust toolchain | ✅ all pass |
| **Integration tests** (`tests/*.rs`, in-process: SQLite temp DBs, temp git repos) | 211 | `git` binary on PATH; a C compiler (bundled SQLite + vendored libgit2) | ✅ all pass |
| **Live-service tests** | 13 | **`handoff-mcp` and `ai-memory` server binaries on PATH** | ❌ **NOT RUN** |

**Explicitly not claimed:** the 13 live MCP tests did **not** pass. They were **skipped by design** (`#[ignore]`). I verified both binaries are **absent from PATH** on this machine (`Get-Command handoff-mcp` and `ai-memory` → not found). Per `docs/PHASE3-MEMORY.md`, they last passed against `handoff-mcp v0.35.1`. They *are* still type-checked, because `--all-targets` compiles every integration target including the ignored ones.

### 3.3 Not verified in this audit

| Item | Why |
|---|---|
| Linux / macOS build | Only Windows validated. **No cross-platform build was attempted.** |
| `cargo build --release` | Only the dev profile was exercised |
| Code coverage | No coverage tooling exists in the project; no coverage figure is claimed |
| `cargo audit` / `cargo deny` | Neither configured |
| Release-profile behaviour, LTO, stripping | Not exercised |
| Live MCP substrate behaviour | Binaries unavailable |

**No command failed, so no failure investigation was required.** No module was modified to make a check pass.

---

## 4. Confirmed Defects and Evidence

Every defect below was **confirmed in this audit** against source. Severity is the auditor's judgement.

### 4.1 High

| ID | Defect | Evidence | Consequence |
|---|---|---|---|
| **D1** | **No continuous integration of any kind** | `.github/` did not exist before this audit | Nothing verifies a contributor's change. Every gate above was run by one person, by hand. *Mitigated in this audit by §6, pending commit.* |
| **D2** | **No runnable artifact** | `cargo metadata`: every target is `lib` or `test`; zero `bin` | Nobody can *use* Orqynn; only compile its tests |
| **D3** | **`director-brain/` divergence carries a silent data-model regression** | its diff removes `state_version += 1` from `Checkpoint::supersede()` (`domain/checkpoint.rs`); committed `main` keeps it | Merging it forward inverts `state_version` semantics on superseded rows, with no merge conflict to warn you |
| **D4** | **`rust-toolchain.toml` pins a triple-specific channel** | `rust-toolchain.toml:2` → `channel = "stable-x86_64-pc-windows-gnu"` | This channel does not resolve on Linux/macOS. A contributor on either platform who does not override it **cannot build the project**. *Handled in CI via `RUSTUP_TOOLCHAIN: stable` (§6); the file itself is unchanged, deliberately.* |
| **D5** | **44 untracked files, including two inside `src/`, none gitignored** | `git status --porcelain`; `.gitignore` covers `**/*.rs.bk` but **not** `*.rs.bak` / `*.rs.original` | `crates/director-app/src/schedule.rs.bak` and `.original` sit inside the module tree; one `git add -A` commits them |

### 4.2 Medium

| ID | Defect | Evidence | Consequence |
|---|---|---|---|
| **D6** | **Phase 13 not implemented** (§5) | `overlaps_scope` has 4 call sites: 1 definition + 4 in `#[cfg(test)]` (`task.rs:425, 426, 436, 443`). Zero production callers | Two agents can be assigned overlapping `scope_paths` and collide silently |
| **D7** | **`TaskSpec` cannot set `scope_paths`** | `plan.rs` builders are `new`/`depends_on`/`expects`/`sized` (`:125, :138, :144, :153`) — **no scope builder** | Every task created through PLAN gets `scope_paths: vec![]`, so even a completed conflict check would have nothing to compare. This blocks Phase 13 independently of the predicate wiring |
| **D8** | **`observer.rs` (639 lines) has zero tests of its own** | no `#[cfg(test)]` / `mod tests` in the file; it calls `Repository::validate_path` at `:86` and `:96` | Security-relevant path validation (`..` rejection, rooted-elsewhere check) is exercised only transitively. `PHASE2-OBSERVATION.md` self-declares this gap |
| **D9** | **Documented `spawn_blocking` remediation never applied** | `executor.rs:13` claims *"Phase 10 wraps this in `spawn_blocking`"*; workspace grep finds `spawn_blocking` only in that comment | `LocalExecutor` blocks the async runtime. Harmless now, a hazard under a daemon |
| **D10** | **`Recovery::is_clean()` always `false`; `actions_available()` always `false`** | `recovery.rs:160` `structural()` hardcodes both flags `true` | Every replacement briefing reports itself degraded; the Phase 12 recovery feature is present but not yet useful |
| **D11** | **`Blocker` has no persistence and is never raised** | no `blockers` table in any migration; `domain/src/blocker.rs` unreachable from the loop | `BlockerKind::Conflict` exists with no way to raise it — Phase 13's natural reporting mechanism is unavailable |
| **D12** | **`ProviderSync` is never written** | `store/src/state_sync.rs` unreached | Substrate drift goes undetected |
| **D13** | **MCP failure cannot affect orchestration state** | `director-app` references the MCP adapters only in its own tests | A dead substrate degrades silently instead of halting — becomes a production hazard at R4 |
| **D14** | **`Probe::{TestSuite, Diff, File}` modelled but always `Unverifiable`** | `engine.rs:265`; `PHASE10` limitations | Verification is materially narrower than the domain model advertises |
| **D15** | **`observe::test_results()` is a stub** | `observe.rs:295` — parameter is `_snapshot`, returns `None` | Observed test results never enter the state machine |

### 4.3 Low

| ID | Defect | Evidence |
|---|---|---|
| D16 | Three documents wrongly state Phase 4 never existed | `PHASE5-STORE.md`, both AUDITs vs `git log -S "Phase 4" -- crates` → `f80184e` created the Phase 4 checkpoint slice inside the "Phase 5" commit |
| D17 | README test badge links to `tests/`, which does not exist at the repo root | `README.md:10` |
| D18 | Contradictory Rust prerequisites | `README.md:235` says 1.85+; `PHASE0-FORENSICS.md` says 1.95+ |
| D19 | Unused workspace dependencies `uuid` and `sha2` | `Cargo.toml:32, 34`; no crate depends on them |
| D20 | `service.rs` cites `tests/git_integration.rs`, which does not exist | `PHASE2-OBSERVATION.md` self-declares: *"The comment overstates coverage that has not been written."* |
| D21 | `PHASE12` doc has a garbled duplicated paragraph | `docs/PHASE12-CHECKPOINT-RECOVERY.md` lines 208–212 |
| D22 | `director-store` lacks `forbid(unsafe_code)` | the other three crates declare it; there are 0 unsafe blocks either way |
| D23 | No git tags, no releases, no CHANGELOG | `git tag` → empty |
| D24 | Unrecognized substrate status silently maps to `VerificationPending` | `handoff/mapping.rs:265, 301` `_ =>` fallbacks. Fails safe, but hides an unknown status |

### 4.4 Defects explicitly *not* found

Stated because the project's history makes them worth checking, and they are **resolved**:

- The `plan.rs` "610 lines never compiled" defect (`AUDIT-2026-09-30.md` F1) — **resolved**; `plan` runs 2 passing tests, and `tests/wiring.rs` now prevents recurrence.
- The uncommitted Phase 7 / Phase 10 work (`AUDIT-2026-10-04.md` F1/F2) — **resolved**; both are ancestors of `origin/main`.
- The inert partial unique indexes (`PHASE5-STORE.md`) — **resolved**; verified by `two_active_assignments_for_one_task_are_rejected` and `creating_a_checkpoint_supersedes_the_previous_one_without_deleting_it`.
- The 111-shared-tempdir test failures — **resolved** by `fd672a4`.

---

## 5. Phase 13 Audit — Deterministic Conflict Detection

### 5.1 Verdict: ❌ **NOT IMPLEMENTED** (ingredients present, decision logic entirely absent)

Phase 13 must not be marked complete on the basis of the existing domain method and unit tests. Both exist; neither is wired.

### 5.2 What exists vs. what is missing

| Requirement | Status | Evidence |
|---|---|---|
| `scope_paths` field on the domain entity | ✅ Present | `domain/task.rs:205` — *"What this task may touch, used for deterministic conflict detection (Phase 13). Advisory for the agent, authoritative for Orqyn."* |
| `scope_paths` persisted to SQLite | ✅ Present | `store/src/tasks.rs:110` (insert), `:332` (update), `:395/:418` (read back) |
| `scope_paths` round-tripped over the MCP wire | ✅ Present | `handoff/mapping.rs:125, 225`; `handoff/wire.rs:76` |
| Overlap predicate | ✅ Present | `Task::overlaps_scope` (`task.rs:296–308`) |
| **Scope-path normalization** | ⚠️ **Partial & buggy** | See §5.3 |
| **Wiring into SCHEDULE eligibility** | ❌ **Missing** | zero matches for `scope`/`conflict`/`overlap` as logic in `schedule.rs` — only two unrelated comments at `:84–85` and `:102` about REPLAN decision conflicts |
| **Wiring into ASSIGN validation** | ❌ **Missing** | zero scope/conflict logic in `assign.rs`; `validate_assignment` (`assign.rs:236–323`) has 8 legality checks, none scope-related |
| **Concurrent in-progress task awareness** | ❌ **Missing** | neither step considers sibling `InProgress` tasks at all |
| **Structured refusal reasons** | ❌ **Missing** | `AssignError` has 9 variants (`app/src/lib.rs`), none scope-related. `UnassignedReason` has 3 (`NoAgentsAtAll`, `NoEligibleAgent`, `AgentTakenEarlier`) |
| **Tests for overlapping scopes** | ⚠️ Unit only | `task.rs:419` `scope_overlap_is_symmetric_and_path_aware` — passes |
| **Tests for non-overlapping scopes** | ⚠️ Unit only | `task.rs:430` `disjoint_scopes_do_not_overlap` — passes |
| **Tests for path edge cases** | ❌ **Missing** | only 3 overlap tests exist (`:419, :430, :440`). No backslash, case, `.`/`..`, leading `./`, root, or empty-scope case |
| **Tests for concurrency** | ❌ **Missing** | no test asserts that two overlapping tasks cannot be simultaneously assigned |
| **Tests for scheduling/assignment behaviour** | ❌ **Missing** | no test in `tests/schedule.rs`, `tests/assign.rs` or `tests/end_to_end.rs` exercises scope |

### 5.3 Correctness defects in `overlaps_scope` itself

The current implementation (`task.rs:296–308`) is:

```rust
pub fn overlaps_scope(&self, other: &Task) -> bool {
    fn normalize(path: &str) -> &str {
        path.trim_end_matches('/')
    }
    self.id != other.id
        && self.scope_paths.iter().any(|a| {
            other.scope_paths.iter().any(|b| {
                let (a, b) = (normalize(a), normalize(b));
                a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
            })
        })
}
```

| # | Defect | Example | Result |
|---|---|---|---|
| **B1** | **Root scope normalizes to the empty string and therefore never overlaps anything** | `a = "src/"`, `b = "/"` → `normalize` gives `"src"` and `""` → `"src" == ""` false; `"src".starts_with("//")` false; `"".starts_with("src/")` false | **`false`, but they genuinely overlap** — `src` is inside the repository root. A repo-wide scope silently conflicts with nothing |
| **B2** | **Empty `scope_paths` means "never conflicts"** | both tasks have `scope_paths: vec![]` → `.any()` over empty is `false` | **`false`.** An unconstrained task — which may touch anything — is treated as conflicting with nothing. This is the *opposite* of the conservative stance the doc comment claims |
| **B3** | **Windows separators not handled** | `"src\\auth"` vs `"src/auth"` | `false`. The project is developed on Windows (`rust-toolchain.toml` pins a Windows triple) |
| **B4** | **Case sensitivity is platform-inconsistent** | `"src/Auth"` vs `"src/auth"` | `false` on all platforms — but on Windows/macOS the real filesystem treats these as the same path |
| **B5** | **`.` and `..` are not resolved** | `"src/auth/../db"` vs `"src/db"` | `false`. They are the same path |
| **B6** | **Leading `./` not stripped** | `"./src"` vs `"src"` | `false` |
| **B7** | **Allocation in a hot path** | `format!("{b}/")` inside the inner `any` closure | minor; `O(n·m)` string allocations per comparison |

### 5.4 Minimal implementation plan (reusing existing APIs, preserving the architecture)

This plan deliberately **adds no new crate, no new store method, and no new domain entity** — the same constraint Phase 7 honoured ("adds no new store method and no new domain entity").

**Step 1 — normalize properly, in `director-domain` (`task.rs`).**
Add a private `normalize_scope_path(&str) -> Option<String>` that: rejects empty/root-after-normalization rather than silently producing `""`; converts `\` to `/`; strips a leading `./`; collapses `//`; resolves `.` and `..` segments; strips trailing `/`; preserves a leading `/` for absolute paths; and yields `None` for a path that normalizes to nothing (fixing **B1**).
Then rewrite `overlaps_scope` to compare normalized component vectors rather than raw prefixes, making the predicate **symmetric by construction** and returning `true` when **either** side is unconstrained and the other is not (**B2**). Keep the existing signature so nothing downstream breaks.
*Platform note:* rather than lowercasing unconditionally (**B4**), do not silently change semantics on Linux — either document the current case-sensitive behaviour explicitly, or compare case-insensitively only on Windows/macOS and test both.

**Step 2 — add a `scope_paths` builder to `TaskSpec` (`plan.rs`).**
`pub fn scopes(mut self, paths: impl IntoIterator<Item = impl Into<String>>) -> Self` alongside `depends_on`/`expects`/`sized`. **This is a hard prerequisite (D7):** without it every planned task has an empty scope and the check has nothing to compare.

**Step 3 — add a structured refusal reason.**
`AssignError::ScopeConflict { task: TaskId, conflicting_task: TaskId, paths: Vec<String> }` in `director-app/src/lib.rs`, plus `UnassignedReason::ScopeConflict { task: TaskId, conflicting_task: TaskId }` in `schedule.rs`. Structured, testable, and consistent with the 9 existing `AssignError` variants.

**Step 4 — enforce in `validate_assignment` (`assign.rs`).**
Insert one check after the existing dependency check (`assign.rs:293–298`), using the `plan_tasks: &[Task]` the function **already receives**:
> for every sibling task in `plan_tasks` whose status is `InProgress`, if `task.overlaps_scope(sibling)` → `Err(AssignError::ScopeConflict { .. })`.

This is a pure function over already-loaded state — no I/O, no clock — matching the function's stated contract at `assign.rs:232–235` (*"No store, no I/O, no clock — everything about the world has already been loaded by `assign_at`"*). **No signature change is required.**

**Step 5 — make SCHEDULE conflict-aware.**
In `schedule_at`, before building the `AssignRequest`, reject tasks whose scope overlaps an already-committed assignment **this round**; report them via `Unassigned::ScopeConflict` rather than letting ASSIGN fail downstream. Add a soft ranking penalty in `match_agents` so a conflict is avoided where possible rather than merely refused.

**Step 6 — persist and surface the conflict.**
Add the `blockers` table as **migration `0004`** with a `BlockerRepository`, closing **D11**. Raise a `Blocker{kind: Conflict}` on a refused scope overlap so the conflict is durable and visible in MONITOR, not only in a return value.

**Step 7 — tests (the currently missing category).**
*Domain:* root scope (**B1**), empty vs. scoped (**B2**), backslash (**B3**), case (**B4**), `..` resolution (**B5**), leading `./` (**B6**), symmetry, self-exclusion, plus property tests over generated path pairs.
*App:* overlapping scopes cannot both be assigned; disjoint scopes always schedule; the refusal carries the right structured reason; an overlapping pair that becomes `InProgress` blocks a later assignment; `end_to_end` proves it across the loop.
*Store:* migration `0004` contiguity; blocker persistence round-trip.
**Rule for Phase 13 to be called complete: a test must fail if `overlaps_scope` is removed from `validate_assignment`.** Today, deleting it changes no test outcome.

**Effort:** 3–5 days. **Risks:** path normalization is deceptively hard (B1–B6); a fuzz test over path pairs is strongly recommended.

---

## 6. CI Status

### 6.1 Before this audit: ❌ **CI did not exist**

`.github/` was **absent**. Zero workflows. Zero automated checks on any push or pull request. All four gates had only ever been run by hand, by one person, on one machine (Windows).

### 6.2 Implemented in this audit

Created **`.github/workflows/ci.yml`** (the only change made to the repository). YAML validated by parse.

**Design decisions, each tied to something verified in this audit:**

| Decision | Why |
|---|---|
| Job-level `env: RUSTUP_TOOLCHAIN: stable` | `rust-toolchain.toml` pins `stable-x86_64-pc-windows-gnu`, a triple-specific channel that does not resolve on Linux/macOS (**D4**). Overriding in CI is the minimal fix — it does **not** edit the pin, so the local Windows workflow for contributors on that platform is preserved |
| `dtolnay/rust-toolchain@stable` with `components: rustfmt, clippy` | Pins the toolchain explicitly rather than inheriting whatever the runner image ships; both gate components are required |
| Matrix: `ubuntu-latest`, `macos-latest`, `windows-latest`; `fail-fast: false` | The project has **only ever been validated on Windows**. Cross-platform breakage is the single largest unverified risk, so a matrix is the point. `fail-fast: false` so one OS failing still reports the others |
| `build-essential` on Linux only | `rusqlite` is `bundled` (C) and `libgit2-sys` vendors libgit2 (C). **`openssl` was deliberately NOT installed**: `Cargo.lock` contains **no `openssl-sys` entry**, so it is not a prerequisite |
| `pkg-config` on macOS only | Standard for the unix C-dependency build path on that image |
| `Swatinem/rust-cache@v2` | Standard; the suite is fast but `rusqlite bundled` + `libgit2-sys` are slow to compile |
| Five separate steps, not one combined step | Each gate reports independently, so a failure names the gate that broke |
| `cargo test --workspace --all-targets` **and** `--doc` as separate steps | `--all-targets` does **not** run doctests; both are needed to reach 608 |
| `permissions: contents: read` | Least privilege; no credentials, no secrets, no write scopes |
| **Live MCP tests deliberately excluded**, with an inline comment explaining why | They require `handoff-mcp` and `ai-memory` binaries this workflow does not install. Adding them would create a permanently-red or silently-skipped job. They remain **type-checked**, because `--all-targets` compiles every integration target |

**What it does not do, deliberately:** no services, no containers, no secrets, no external deploy steps, no release automation, no `cargo audit`/`cargo deny` (recommended separately — they need policy files the project does not have yet).

**Expected first-run outcome:** ⚠️ Linux and macOS may fail on first execution, most likely in the C-dependency build step. That is a **feature** — it surfaces a portability gap that has been invisible until now, and it is exactly why the matrix exists.

### 6.3 Not done in this audit, and why

`SECURITY.md`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, issue/PR templates, `cargo deny` policy and Dependabot were **not** created. They are valuable, but Step 5 scoped this task to CI, and each requires project-policy decisions (disclosure channel, contribution norms, licence policy) that are the author's to make. They are listed as recommendations in §7.

---

## 7. Prioritized Next-Step Plan

### Priority A — Stabilization *(do now; ~1 week)*

| # | Action | Status | Effort |
|---|---|---|---|
| A1 | **Retire `director-brain/`** — confirm nothing unique survives, then remove. **Do not merge it forward** | Recommended | ~1 h |
| A2 | **Clear the 44 untracked files**; delete `src/schedule.rs.bak` and `.original` or extend `.gitignore` to cover `*.rs.bak` / `*.rs.original` | Recommended | ~30 min |
| A3 | **Commit `.github/workflows/ci.yml`** and let the matrix report its first result | Workflow written, **awaiting commit** | ~5 min + first run |
| A4 | **Complete Phase 13** per §5.4 — add `TaskSpec::scopes`, normalize paths, enforce in `validate_assignment`, make SCHEDULE conflict-aware, add migration `0004` + blockers, add the missing tests | Plan defined | 3–5 days |
| A5 | **Resolve the LICENSE copyright holder** — currently *"Copyright (c) 2026 Orqyn contributors"*, which is not a legal entity. Author's legal call; this audit asserts no ownership and modified nothing | Recommended | ~1 h |
| A6 | Fix the low-cost defects: D16–D22 (docs, badge path, unused deps, `forbid(unsafe_code)` on `director-store`) | Recommended | ~1 h |
| A7 | Add `SECURITY.md` + `CONTRIBUTING.md` (publishing priority for the former, since `director-app` executes task-defined commands) | Recommended | 1–2 days |

**Phase 13 is genuinely incomplete** and is the one item in Priority A that is real feature work rather than hygiene. It should be finished before any new phase begins, because it is the last missing core invariant.

### Priority B — Runnable MVP

**Question posed:** should a CLI or integration runner come before a daemon and Orqynn MCP server?

**Recommendation: an integration runner first, then a thin CLI, then the daemon. Not the daemon first.**

Reasoning, based on the verified code rather than on preference:

1. **The orchestration steps are all pure async functions over already-injected dependencies** — `observe(&Store, &GitService, &mut IdGenerator, …)`, `plan(&Store, PlanRequest)`, `schedule(&Store, &mut IdGenerator, …)`. A loop driver is therefore *mechanically trivial*; the hard part is everything around it (config, shutdown, concurrency, substrate failure handling, observability).
2. **The most valuable thing a first runner does is destructive testing.** Phase 11 proves crash survival **in-process** and admits *"No process boundary, so no true kill test."* A minimal runner that can be spawned and `SIGKILL`ed at N injected points converts every durability claim from an assertion into a demonstration — and it becomes the seed of the daemon rather than throwaway scaffolding.
3. **A daemon without a CLI is un-operable; a CLI without a runner has nothing to drive.** The ordering runner → CLI → daemon means each layer is testable by the one above it.
4. **Avoid the abstractions you don't need yet.** A `LoopDriver` trait, a plugin architecture, or a config framework would all be premature. The smallest useful runner is a `main.rs` that calls the seven functions in order on a tick, with a `tokio::signal::ctrl_c()` shutdown.

**Concrete Priority B sequence:**

1. **B1 — Integration runner** (new crate or `director-app` bin, ~1 week). Sequentially invokes the seven steps, prints a per-round summary, exits cleanly on a signal. Critically: it must be *spawnable as a child process* so kill-point tests become possible. This closes Phase 11's acknowledged gap.
2. **B2 — End-to-end kill-point tests** (~3–5 days). Spawn the runner, `SIGKILL` at injected points (mid-`create_active_plan`, mid-`assign_task`, mid-`checkpoint_round`, mid-`apply_verification`), reopen the SQLite file and assert: no partial transaction, no duplicate `AssignmentId`, no re-minted `CheckpointId`, no task in `Done` without a `Verification` row. Promote Phase 11's matrix A–L from prose into executed assertions. *Watch out:* `SIGKILL` semantics differ on Windows (`TerminateProcess`) — this is the hardest part.
3. **B3 — Thin CLI** (~2–3 days, **after** B2 proves the runner). `init`, `project add`, `plan show`, `task list`, `run once`, `run watch`, `brief <task>`, `doctor`. `--json` on every command. `brief` is the visible payoff of Phase 12's recovery work. Hold off on `daemon start/stop` until B4 exists.
4. **B4 — Daemon** (P0 for the daemon and MCP server, but genuinely P1 relative to B1–B3). Add interval-driven operation, graceful drain on `SIGTERM`, config file, per-project registration, concurrency limits. Only now does "Orqynn MCP server" become worth building — it is a thin `tools/list` surface over a daemon that is already observable and testable.
5. **B5 — Wire the MCP adapters into the runner** to close **D13** (a substrate outage currently degrades silently). This must happen before anyone runs Orqynn unattended.

**Minimum credible MVP = B1 + B2 + B3.** That yields a command a person can run, a crash-durability guarantee that is demonstrated rather than asserted, and a visible recovery briefing — from a library that today has none of the three.

### Priority C — Phase 9: RecentContext Engine

**Recommendation: DEFER, but plan it now; do not implement it before B1–B3.**

The constraint from the instruction — *"avoid duplicating task state or creating a competing memory store"* — is exactly right, and the code already respects it. `RecentContext` (`domain/context.rs`) is a **bounded window of recent actions** (`DEFAULT_MAX_ACTIONS = 50`), explicitly **not** a task-state store; `Action` (`domain/action.rs`) carries `ActionKind`, a summary, and an optional importance rank. Nothing about it competes with `tasks`, `plans`, `decisions` or `checkpoints`.

**Integration path (should it be built at all?):** yes — it is what closes **D10**. `RecoveryGaps::structural()` currently hardcodes `recent_context_unavailable: true` and `blockers_unavailable: true`, which is why `Recovery::is_clean()` is `false` for **every** build. A minimal, persistent, agent-independent context model would let `RecoveryBuilder` report a *real* gap state instead of a constant, and would give a replacement agent the "what just happened" half of its briefing — currently it gets only "what is true".

**Minimal model, if/when built:**
- **New table via migration `0004` (alongside Phase 13's blockers) or `0005`:** `task_actions { id, task_id, project_id, session_id, agent_id, kind, summary, importance, occurred_at, state_version }`.
- `RecentContext::push` gains a store-backed sibling; `ContextSnapshot::from_window` is already the right read shape and needs no change.
- Actions are written by **Orqyn itself**, at the points where it already makes a decision (assign, lease expiry, verification verdict, replan remediation) — never by an agent. That is what makes it "agent-independent".
- **Bounded retention** is mandatory: the existing window semantics (`respects_bound`, `DEFAULT_MAX_ACTIONS`) should be enforced by a trim query, or the table reproduces the unbounded-growth problem that `git/store.rs` already has (D: no event retention policy).
- It must be wired into `ContinuationPackage` behind the existing `recent_context_unavailable` flag, which is already plumbed — the seam exists.

**Why defer:** Phase 9 improves the *quality* of a recovery briefing. B1–B3 make a recovery briefing *exist*. Sequencing quality before existence is backwards.

### Priority D — Phase 8: Multi-Identity Agent Pool

**Recommendation: DEFER aggressively. It is not needed for the first runnable MVP, and building it now would be actively harmful.**

**Verified current constraint:** `PHASE2-ADAPTERS.md:120` promised *"The pool that manages several identities is Phase 8"*, and six source comments still reference it (`handoff/adapter.rs:409`, `handoff/transport.rs:17`, `domain/agent.rs:62`, `domain/capability.rs:4`, `domain/handoff.rs:1`, `adapters/memory.rs:578`). `HandoffAdapter` holds **one** `AgentId` for its lifetime (`adapter.rs:192` `agent_identity()`), so **one adapter instance speaks as one agent**.

**What is actually necessary for the first runnable MVP: essentially none of it.** Decomposed:

| Capability | Needed for B1–B3? | Reasoning |
|---|---|---|
| Multi-identity pool over one substrate process | **No** | The MVP runs against Orqynn's own store. A single registered agent is enough to exercise PLAN → SCHEDULE → ASSIGN → MONITOR → VERIFY → REPLAN end to end |
| Capability matching | **Already built** | `Capability::matches` (`capability.rs:64`) + `AssignError::AgentMissingCapabilities` are implemented and tested |
| Agent lifecycle management | **Already built** | `AgentStatus`/`Liveness` (`agent.rs:65, 111`), `status_from_heartbeat()`, `heartbeat()`, `can_accept_work()` |
| Handoff between agents | **No** | Handoff is the substrate-facing transfer protocol. Orqynn's own model already handles reassignment through `AgentAssignment` tenure supersession — verified by `assignment_history_retains_every_tenure` |
| Spawning real agent runtimes (Claude Code, Codex, …) | **No** | A `Harness` is currently just a label. The MVP does not need to *launch* anything; the verification path runs commands, not agents |

**What genuinely requires Phase 8 later:** talking to `handoff-mcp` as more than one identity — which only matters once Orqynn is driving a *real fleet* through a substrate. That is post-MVP by definition.

**Why building it now would be harmful:** the pool's value depends on a working end-to-end workflow to distribute across. Without B1, a pool is a component with no consumer — and the project's own history shows what happens to unreached code: `director-store` still carries `#![allow(dead_code)]` for six helpers written but never called (**D12** `ProviderSync` being the clearest example). Adding a pool now would very likely add a seventh.

**One genuinely cheap piece, if wanted:** fix the dangling promise. Either implement the pool, or amend the six source comments and `PHASE2-ADAPTERS.md:120` to say the multi-identity pool is deferred to Phase 8 of the roadmap with a pointer. A promise that has sat unaddressed since 2026-09-25 is a documentation liability even though the code is honest about the limitation.

### 7.1 Recommended sequencing

```
Priority A (now, ~1 week)
  A1 retire director-brain     A2 clear untracked    A3 commit CI
  A4 COMPLETE PHASE 13         A5 LICENSE holder      A6 small defects    A7 SECURITY/CONTRIBUTING
        |
        v
Priority B (~3 weeks)   B1 runner -> B2 kill-point tests -> B3 CLI -> B4 daemon -> B5 wire MCP adapters
        |                                              (B4 = Phase 18 daemon + Orqynn MCP server)
        |                                              (B5 = Phase 17 minimum viable)
        |
        +--> Phase 9 RecentContext   (only after B1-B3; closes D10)
        |
        +--> Phase 8 Agent Pool      (only when a real fleet is driven through a substrate)
```

---

## 8. Explicit Phase 8 and Phase 9 Recommendation

| Phase | Current state (verified) | Recommendation | Rationale |
|---|---|---|---|
| **Phase 8 — Multi-Identity Agent Pool** | ❌ Not implemented. `HandoffAdapter` holds one `AgentId`; six source comments plus `PHASE2-ADAPTERS.md:120` still promise the pool. Capability matching and agent lifecycle are **already built and tested**, so the genuinely missing piece is only identity multiplexing | **DEFER. Not required for the first runnable MVP.** Do not build identity multiplexing until Orqynn drives a real multi-agent fleet through a substrate. Consider amending the six dangling comments to point at the roadmap | Pool value depends on a working end-to-end workflow to distribute across. Building it before B1 creates unreached code — the exact failure mode `director-store`'s `#![allow(dead_code)]` already demonstrates. Two of its four sub-capabilities (capability matching, lifecycle) are already done, so the deferral costs nothing today |
| **Phase 9 — RecentContext Engine** | ❌ Not implemented. `RecentContext` (`domain/context.rs`) and `Action` (`domain/action.rs`) exist as in-memory types; **no table, no repository, never called by the loop**. `PHASE12` confirms: *"RecentContext has no table and no repository"* | **DEFER implementation, but DESIGN IT NOW.** Build it immediately after B1–B3. It is the documented cause of **D10** (`Recovery::is_clean()` is `false` for every build) and it completes the replacement-agent briefing | It is genuinely valuable and correctly scoped — it does not duplicate task state, and the `recent_context_unavailable` seam is already plumbed through `ContinuationPackage`. But it improves a briefing that B1–B3 must first make *exist*. Quality before existence is backwards |

**Explicit note:** neither Phase 8 nor Phase 9 was implemented during this audit. No source file in the repository was modified — `git diff --stat HEAD` is empty.

---

## 9. Reproducible Commands

Environment: `rustc 1.98.1 (48a229cea 2026-09-01)`, `cargo 1.98.1`, toolchain `stable-x86_64-pc-windows-gnu`, Windows. Full suite runtime: well under one minute.

```powershell
git clone https://github.com/abyyxhek/orqynn.git
cd orqynn

# ---- the four gates (all exit 0 on 65cfa02) -------------------------
cargo fmt --all --check                                   # exit 0
cargo clippy --workspace --all-targets -- -D warnings     # exit 0, zero warnings
cargo build --workspace                                   # exit 0
cargo test --workspace --all-targets                      # 606 passed, 0 failed, 13 ignored
cargo test --workspace --doc                              #   2 passed, 0 failed  => 608 total

# ---- targeted Phase 12 / Phase 13-relevant suites ---------------
cargo test -p director-app --test checkpoint_lineage        #   1  - the re-minted CHK-work-1 regression
cargo test -p director-app --test recovery_builder          #  18  - continuation packages
cargo test -p director-app --test runtime_recovery          #   5  - Phase 11 crash/rebase/briefing proofs
cargo test -p director-app --test end_to_end                #   2
cargo test -p director-app --test schedule                  #  13  - where Phase 13 must add coverage
cargo test -p director-app --test assign                    #  15  - where Phase 13 must enforce
cargo test -p director-domain task::tests                   # scope overlap unit tests (task.rs:418-444)

# ---- confirm there is no runnable artifact -----------------------
cargo metadata --format-version 1 --no-deps                 # every target is lib:* or test:*

# ---- live MCP suites: NOT RUN here, binaries absent -------------
# Requires `handoff-mcp` and `ai-memory` on PATH.
cargo test --workspace -- --ignored                         # 13 tests: 8 ai-memory + 5 handoff
```

**Windows + OneDrive note (from the README):** if the checkout lives inside OneDrive, redirect the build directory — `$env:CARGO_TARGET_DIR = "$HOME\.orqyn-target"`.

---

## 10. Changes Made by This Audit

| Path | Change | Authorization |
|---|---|---|
| `.github/workflows/ci.yml` | **New** — 3-OS matrix, five gates | Step 5, explicitly authorized |
| `ORQYNN_NEXT_STEP_AUDIT.md` | **New** — this deliverable | Requested |
| — | **No source file modified.** `git diff --stat HEAD` is empty | Constraint respected |
| — | **Nothing committed, pushed, reset, cleaned, stashed or merged** | Constraint respected |

---

## 11. Evidence Index

| Claim | Evidence |
|---|---|
| Repository identity | `git remote -v`; `git rev-parse HEAD`; `Cargo.toml:15`; `README.md:468` |
| Clean tree / 44 untracked | `git status --porcelain`; `git diff --stat HEAD` (empty) |
| Alternate checkouts | `git -C <dir> remote get-url origin` across 5 sibling directories; `git worktree list` |
| Stash and refs | `git stash list`; `git for-each-ref` |
| `director-brain` divergence | `git -C director-brain status -sb`; `git -C director-brain diff --stat` (6 files, +318/−20) |
| No binary anywhere | `cargo metadata --no-deps` — every target `lib:*` or `test:*` |
| Four crates + lint posture | 4 × `Cargo.toml`; `domain/lib.rs:47–48`; `adapters/lib.rs:34–35`; `app/Cargo.toml [lints.rust]`; `store/lib.rs:59` |
| Loop steps | `observe.rs:141`; `checkpoint.rs:436`; `plan.rs:188`; `schedule.rs:130`; `assign.rs:107`; `monitor.rs:169`; `verify.rs:266`; `engine.rs:115`; `replan.rs:379` |
| No loop driver | grep `run_loop\|daemon\|tokio::main\|fn main\|clap\|interval(` across `src/` → 0 production hits; `app/lib.rs:39–44` |
| Optimistic concurrency | `connection.rs:224` `row_count_to_outcome`; test `two_concurrent_writers_on_the_same_version_see_one_winner_and_one_conflict` |
| WAL/FK verified | `connection.rs:169–192` |
| Migrations | `migrations.rs:63–235`; `migrations/000{1,2,3}_*.sql` |
| Checkpoint lineage | `checkpoint.rs:211` `should_checkpoint`, `:255` `restates_current`, `:436` `checkpoint_round`; `tests/checkpoint_lineage.rs` |
| Recovery gaps | `recovery.rs:160` `RecoveryGaps::structural()` |
| Zero self-reported completion | `task.rs:149–155`; production `TaskStatus::Done` write sites = `verify.rs:444`, `verification.rs:235` only; `mapping.rs:81` `trusted_done_ids` |
| Timeout handling | `executor.rs:24, 56, 101, 120, 202–223` |
| `spawn_blocking` never applied | workspace grep → only `executor.rs:13` doc comment |
| Boundary/wiring tests | `tests/boundary.rs` (1 pass), `tests/wiring.rs` (3 pass) |
| `scope_paths` plumbing | `task.rs:205`; `store/tasks.rs:110, 332, 395, 418`; `handoff/mapping.rs:125, 225` |
| `overlaps_scope` uncalled | `task.rs:296` definition; call sites only at `:425, 426, 436, 443`, all inside `#[cfg(test)]` |
| No scope logic in steps | grep `scope\|conflict\|overlap` in `schedule.rs` → comments only (`:84–85, :102`); in `assign.rs` → `:214` only |
| `TaskSpec` lacks a scope builder | `plan.rs:125, 138, 144, 153` |
| `observer.rs` untested | no `#[cfg(test)]`/`mod tests`; calls `Repository::validate_path` at `:86, :96` |
| Live tests skipped | `#[ignore]` at `aimemory_live.rs:71–233` (8), `handoff_live.rs:80–213` (5); `Get-Command` → both binaries absent |
| No CI before this audit | `Test-Path .github` → False |
| CI written | `.github/workflows/ci.yml`, YAML parse-verified |
| MSRV vs installed toolchain | `Cargo.toml` `rust-version = "1.85"`; `rustc --version` → 1.98.1 |
| OpenSSL not required | `Cargo.lock` contains `cc`, `libgit2-sys`, `libz-sys`, `pkg-config`, `vcpkg` — **no `openssl-sys`** |
| Dangling Phase 8 promise | `PHASE2-ADAPTERS.md:120` + 6 source comments; `handoff/adapter.rs:192` single `agent_identity()` |
| Phase 4 existed | `git log -S "Phase 4" -- crates` → `f80184e` (created the checkpoint slice inside the "Phase 5" commit), `bda3382` |
| Phase 9 never built | `domain/context.rs`; `PHASE12-CHECKPOINT-RECOVERY.md` limitations |
