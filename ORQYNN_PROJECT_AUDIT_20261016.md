# Orqynn (Director Brain) — Project Audit, Development History & Open-Source Readiness Report

**Audit date:** 2026-10-16
**Auditor:** opencode (automated, source-and-history-driven repository audit)
**Repository audited:** `https://github.com/abyyxhek/orqynn.git` (remote `origin`)
**Canonical local path:** `C:\Users\ASUS\OneDrive\Desktop\ai_memory\orqyn`
**HEAD audited:** `65cfa02eb534d7e77533891a1d9f3a882d1d77ea` — *"docs: sync README with the real API; pin the example with a compiled test"* (2026-10-14)

**Method.** Every claim below was derived by reading the actual source tree, the actual Git history, and the actual documentation in `docs/`, and by executing the build, test and lint gates on this working tree. Claims that could not be verified by execution are labelled **NOT VERIFIED**. Claims inherited from earlier audits (notably `docs/AUDIT-2026-09-30.md`, `docs/AUDIT-2026-10-04.md`, and the pre-existing untracked `ORQYNN_PROJECT_AUDIT.md`) are labelled as prior claims and were re-tested where possible.

> **Naming note.** This report covers **Orqynn** only. It is *not* the similarly-named pre-rebrand working name "Orqyn" (which survives inside this tree as the `crates/director-*` crate names and the `aimemory`/`handoff` adapter module names — a historical artifact, not a separate project) and has no relationship to "SNAGLINE".

> **Non-destructive audit.** Nothing was reset, cleaned, stashed, overwritten, committed or pushed. No source file was modified. Two new untracked report files were created (§8).

---

## 1. Executive Summary

Orqynn is a **library-grade orchestration core with no runnable shell**. The internal engineering quality is genuinely high — better than most production Rust codebases — and, unusually, the *correctness reasoning* is documented at the level of individual invariants and their proofs. What is missing is the entire outer layer that turns a library into a product: there is no executable, no CLI, no daemon, no Orqynn MCP server, no CI, no release, and no contributor onboarding. An external developer can compile and run the test suite, but cannot *use* Orqynn without writing Rust code of their own that calls the library.

**Verified state at a glance**

| Metric | Value | How verified |
|---|---:|---|
| Git commits on `main` | 43 | `git log --oneline \| Measure-Object` |
| Commit date span | 2026-09-25 → 2026-10-14 | `git log --format=%ad --date=short` |
| Branch / ahead / behind | `main`, **0 ahead / 0 behind** `origin/main` | `git status -b` |
| Tracked modifications | **None** | `git status --porcelain` |
| Untracked files | 42 | `git status --porcelain` |
| Git tags / releases | **0** | `git tag \| Measure-Object` |
| Commit authors | **1** (`Abishek`) | `git shortlog -sne --all` |
| Crates in workspace | **4** | `Cargo.toml` members |
| Rust source files (`src/`) | **63** | filesystem scan |
| Rust source LOC (`src/`, incl. inline `#[cfg(test)]`) | **26,298** | line count |
| Rust production LOC (excl. `#[cfg(test)]` modules) | **~20,341** | line count |
| Test files (`tests/`) | **17** | filesystem scan |
| Test LOC | **10,868** | line count |
| Test attributes (`#[test]` / `#[tokio::test]`) | **619** (408 inline + 211 in `tests/`) | grep |
| **Tests passing** | **608** (606 unit/integration + 2 doctests) | **`cargo test --workspace --all-targets` + `--doc`, executed** |
| **Tests failing** | **0** | same |
| Tests ignored (live MCP suites) | **13** (8 ai-memory + 5 handoff) | same |
| `cargo fmt --all --check` | **Clean** (exit 0) | executed |
| `cargo clippy --workspace --all-targets -- -D warnings` | **Clean** (exit 0) | executed |
| SQL migrations | **3** (258 + 57 + 36 = 351 lines) | `crates/director-store/migrations/` |
| SQLite tables | 15 + `schema_migrations` | `0001`–`0003` |
| Index statements | 26, of which **3 partial-unique invariant guards** | `0001`–`0003` |
| Documentation | 13 markdown files, **3,585 lines** | `docs/` |
| Lockfile crates (transitive) | 103 | `Cargo.lock` |
| **Binary targets / `main.rs` / `examples/` / `benches/`** | **0 / 0 / 0 / 0** | scan of every `Cargo.toml` + tree |
| **CI workflows (`.github/`)** | **0 — the directory does not exist** | `Test-Path .github` → False |
| `unsafe` blocks | **0** | grep (3 crates declare `forbid(unsafe_code)`) |
| `todo!` / `unimplemented!` / `FIXME` in `src/` | 0 / 0 / 0 | grep |
| Secret/credential handling in `src/` | **0 matches** | grep |

**Bottom line for a senior reviewer.** This is a **library-quality alpha**: the orchestration core — the part that is genuinely hard and genuinely novel — is built, tested, lint-clean and internally consistent. But it has **no runnable artifact**, so it is not yet a usable product. It is *publicly visible* on GitHub; it is **not yet ready for external contributions** and is **not remotely production-ready**.

---

## 2. Repository Verification

### 2.1 Identity

| Property | Value |
|---|---|
| Remote (fetch) | `https://github.com/abyyxhek/orqynn.git` |
| Remote (push) | `https://github.com/abyyxhek/orqynn.git` |
| Git root | `C:/Users/ASUS/OneDrive/Desktop/ai_memory/orqyn` |
| `Cargo.toml` `repository` | `https://github.com/abyyxhek/orqynn` |
| README published-at | `github.com/abyyxhek/orqynn` |
| LICENSE | MIT, "Copyright (c) 2026 Orqyn contributors" |

✅ **Confirmed: this is `abyyxhek/orqynn`.** All three independent identity signals (git remote, Cargo manifest, README) agree.

⚠️ **Local directory name is `orqyn`, not `orqynn`** (one `n` short). This is a local filesystem artefact only; the remote and manifest are correct. It is a persistent confusion risk — see §9.2.

### 2.2 Working-tree state (preserved, not modified)

- `main` is **exactly in sync** with `origin/main` (0 ahead, 0 behind) at `65cfa02`.
- **No tracked file is modified.** The working tree is clean.
- **42 untracked files**, all of which are audit/test scratch artefacts plus two report files (§8). They are *not* covered by `.gitignore` — see §7.1.

### 2.3 Sibling worktree divergence — HIGH RISK, ACTION REQUIRED

A second checkout of the same remote exists at `../director-brain`:

| Property | `orqyn/` (canonical) | `director-brain/` (diverged) |
|---|---|---|
| HEAD | `65cfa02` (2026-10-14) | `37f39b8` (2026-10-10) |
| vs `origin/main` | 0 / 0 | **ahead 1, behind 2** |
| Phase 12 (`bda3382`) | present | **absent** |
| Tracked modifications | none | **6 files, +318 / −20 lines** |
| Untracked source | none | `crates/director-app/src/checkpoint.rs` (636 lines) |

**What the in-flight work is.** It is the **Phase 12 automatic-checkpoint writer at an earlier iteration than the version already committed on `orqyn/main`**. Its untracked `checkpoint.rs` exposes `auto_checkpoint(...)` (not `checkpoint_round`), mints ids from `&mut IdGenerator`, and lacks `should_checkpoint`, `restates_current`, `CheckpointConflict`, and lineage-derived `CHK-<task>-N` ids. `docs/PHASE12-CHECKPOINT-RECOVERY.md` describes exactly the defects this intermediate version still has ("the old code re-minted `CHK-work-1` for a task that already had it and the whole observation round failed on `checkpoints.id`'s primary key").

**The specific danger.** The uncommitted diff **removes `self.state_version += 1` from `Checkpoint::supersede()`** (renaming the test to `..._marks_it_and_keeps_its_version`), whereas committed `main` **keeps** the increment. Applying the in-flight patch onto `main` would therefore be a *silent data-model regression* — not a merge conflict. It would also revert the README from "Phases 1–3, 5–7, 10–12 Complete / 608 Passed" back to "10–11 Complete / 538 Passed" and drop the "Amnesiac Replacements" failure-mode section.

**Recommendation:** treat `../director-brain` as read-only archaeology. Do **not** merge it forward. Confirm nothing unique survives, then retire the worktree. This is a decision for the author; the audit did not touch it.

### 2.4 Untracked file inventory

| Group | Files |
|---|---|
| Test/build scratch logs | ~36 `*.txt` (`full_test_output.txt`, `builderror.txt`, `test_output*.txt`, `end_to_end_output.txt`, …) |
| Workbooks | `Director_Brain_Current_Status_and_Roadmap.xlsx`, `ORQYNN_PROJECT_AUDIT.md`, `ORQYNN_PROJECT_AUDIT.xlsx` |
| Scripts | `create_workbook.py`, `agent_fee.txt` |
| In-tree leftovers | `crates/director-app/src/schedule.rs.bak`, `crates/director-app/src/schedule.rs.original` |

`crates/director-app/src/schedule.rs.bak` and `.original` sit **inside `src/`** and are **not gitignored** (`.gitignore` covers `**/*.rs.bk`, not `*.rs.bak`/`*.rs.original`). A `git add -A` would commit them. See §7.1.

---

## 3. Development History & Phase-by-Phase Status

### 3.1 Verified commit timeline (43 commits, 2026-09-25 → 2026-10-14)

| # | Hash | Date | Subject |
|---|---|---|---|
| 1 | `2502a92` | 09-25 | Phase 0-1: forensics audit, canonical domain model, provider boundary |
| 2 | `f5dd9b6` | 09-25 | Phase 2 (partial): handoff-mcp wire types, stdio transport, mapping |
| 3 | `80e24ed` | 09-25 | Phase 2 (git half): read-only git observer, JSON store, composing service |
| 4 | `c8eb03b` | 09-25 | README: record the Phase 2 git observation layer as landed |
| 5 | `22141f9` | 09-25 | docs: PHASE2-OBSERVATION |
| 6 | `62e1c1c` | 09-25 | handoff: read session ownership, lineage, workdir from the summary |
| 7 | `4790ed3` | 09-25 | **Phase 2 (complete)**: HandoffAdapter — the provider traits made real |
| 8 | `bb4ce93` | 09-27 | README: record Phase 2 as complete |
| 9 | `ec1878f` | 09-28 | README: rebrand project to Orqyn |
| 10 | `947b293` | 09-29 | Add MIT LICENSE; finish the Orqyn rebrand in docs |
| 11 | `1218d70` | 09-27 | **Phase 3 (complete)**: AiMemoryAdapter — MemoryProvider made real |
| 12 | `f80184e` | 09-28 | **Phase 5 (complete)**: director-store — persistence for Director's own entities |
| 13 | `98f1e29` | 09-28 | store: wire `assign_task` through `Store`; fix stale departing-agent view |
| 14 | `13aa7c3` | 09-29 | Reconcile onto the Orqyn rebrand; finish the rename in prose |
| 15 | `b265c5d` | 09-29 | **Phase 5**: plans and decisions — the superseding artifacts persist (migration `0002`) |
| 16 | `69258fa` | 09-29 | Phase 6: director-app — the loop's OBSERVE step |
| 17 | `7e60627` | 09-29 | docs: record Phase 5 completion, Phase 6 underway |
| 18 | `d57098f` | 09-30 | Phase 6: dependency-graph validation in director-domain |
| 19 | `96c2e14` | 09-30 | Phase 6: store creates a plan and its tasks, and activates it, in one transaction |
| 20 | `3cd4c06` | 09-30 | Phase 6: the PLAN step |
| 21 | `cf2231e` | 09-30 | test: fail the build when a source file is not wired into the module tree |
| 22 | `4c934af` | 09-30 | docs: README reflects the codebase + the 2026-09-30 audit |
| 23 | `b853602` | 10-01 | Phase 6: the ASSIGN step |
| 24 | `405fa9b` | 10-02 | Phase 6: the MONITOR step |
| 25 | `ca688a2` | 10-02 | docs: README status catches up with MONITOR |
| 26 | `3459c33` | 10-02 | Phase 6: the VERIFY step |
| 27 | `32b7f87` | 10-02 | Phase 6: the REPLAN step |
| 28 | `17ed5d0` | 10-02 | fix: close Phase 6 REPLAN consistency gaps |
| 29 | `2e1f4fa` | 10-02 | **Phase 7**: the SCHEDULE step |
| 30 | `f3321b5` | 10-02 | docs: Phase 7's scheduler |
| 31 | `1a166df` | 10-02 | docs: stop calling the planner "Phase 7" — that number is the scheduler |
| 32 | `ad9dead` | 10-05 | **Phase 10 (record layer)**: durable record of judging a task's work |
| 33 | `ec64034` | 10-05 | fix: wire the verification column list into every read, restore fmt |
| 34 | `2d7b213` | 10-07 | **feat: implement phase 10 verification engine** |
| 35 | `a9b0a7b` | 10-07 | docs: document phase 10 verification engine |
| 36 | `fd672a4` | 10-07 | fix: isolate git test fixture temp directories |
| 37 | `373cfc4` | 10-07 | **test: validate phase 11 runtime recovery** |
| 38 | `e016ec3` | 10-07 | docs: document phase 11 runtime recovery |
| 39 | `f5ba150` | 10-07 | README: point at the canonical orqynn repo |
| 40 | `a5e464b` | 10-09 | docs: expand README with architecture & workflow diagrams |
| 41 | `250cc79` | 10-09 | docs: add visual architecture banner to README |
| 42 | `bda3382` | 10-13 | **feat: Phase 12 — checkpointing inside OBSERVE + recovery context** |
| 43 | `65cfa02` | 10-14 | docs: sync README with the real API; pin the example with a compiled test |

### 3.2 Phase-by-phase status

Status legend: **Complete** = objective met and evidence present in the tree · **Partially Complete** = core landed, documented scope remaining · **Planned** = documented but not built · **Unverified** = claimed without inspectable evidence.

| Phase | Name | Documented objective (abridged) | Implemented — evidence | Tests / evidence | Status | Remaining acceptance criteria & known defects |
|---:|---|---|---|---|---|---|
| **0** | Repository Forensics | Read-only audit of `handoff-mcp` and `ai-memory`; establish the substrate boundary | `docs/PHASE0-FORENSICS.md` (724 lines); set the "Orqyn must implement itself" list (§7.4: checkpoint engine, verification engine, planner/replanner, recent-context engine, recovery, multi-agent conflict detection, cross-machine continuity, own persistence) | Not applicable (a report) | **Complete (as a report)** | Doc itself records: neither substrate was built or run; runtime behaviour unverified; JS workflow libs inventoried but not line-reviewed |
| **1** | Domain Model & Provider Boundary | Canonical domain entities, sealed newtype IDs, provider traits, **zero substrate coupling** | `crates/director-domain/**` — 21 modules; 16 sealed ID newtypes (`ids.rs:99–214`); 7 provider traits (`providers.rs:148–303`); 11 store repository traits (`store.rs:216–510`) | `director-domain` suite: **168 tests passing** (executed). Definition-of-done list in `PHASE1-DOMAIN.md` is all-`[x]` | **Complete** | Deliberately no persistence (→Phase 5), no MCP server, no planner, no loop, no adapters (→Phases 2/3) |
| **2** | Adapters & Observation | Give Phase 1's traits "something that is not a `HashMap`" — `HandoffAdapter` over live `handoff-mcp` (MCP half) + a read-only `git2` observation layer (git half) | `handoff/{adapter,mapping,transport,wire}.rs` (2,378 lines); `git/{observer,service,store}.rs` (1,568 lines); `director-domain/src/repository.rs` (1,261 lines); `git2 = "0.21"` | `director-adapters` unit: **77 passing**; live suites **13 ignored** (never run in this audit — no substrate binaries) | **Complete** | Doc-acknowledged: (a) the `extra` MCP channel is closed in both directions, so an Orqyn-only status and `Critical` priority do **not** survive a round trip; (b) identity is process-global — one adapter instance = one agent; (c) `set_agent_status` unimplemented at the substrate; (d) **`observer.rs` has no tests of its own** — its path-validation rules (the `..` rejection, the rooted-elsewhere check) are security-relevant and only exercised transitively; (e) `service.rs` claims coverage from `tests/git_integration.rs` — **that file does not exist**; (f) no event-retention policy (events accumulate unbounded) |
| **3** | Memory Substrate | Second real implementor of `MemoryProvider`: `AiMemoryAdapter` over live `ai-memory` | `aimemory/{adapter,transport,wire,mod}.rs` (1,254 lines); `stdio_mcp.rs` (306 lines, shared MCP transport) | Adapter unit tests green; 8 live ai-memory tests `#[ignore]`d | **Complete** | Doc-acknowledged: `MemoryQuery::tags` is **ignored, not honored**; `kind` round-trips only when it names a tier (a `decision` kind is dropped on write) |
| **4** | *(no document)* | — | **Folded into Phase 5.** `git log -S "Phase 4" -- crates` returns exactly two commits: `f80184e` (which created `director-domain/src/{checkpoint,store}.rs` and `director-store/src/checkpoints.rs`, all in a commit labelled "Phase 5") and `bda3382` (which reworded one comment). | See Phase 5 | **Numbering anomaly — see §3.3** | Three documents (`PHASE5-STORE.md`, both AUDITs) claim "**No Phase 4 … Deliberate.**" The code evidence shows Phase 4 *did* exist as the checkpoint/persistence slice and was folded in, not skipped. **The documentation is wrong on this point.** |
| **5** | Durable Store | A store for Orqyn's *own* entities — those no substrate owns and that must never be delegated to a provider trait | `crates/director-store/**` — 14 modules; `Store` facade with 11 sub-accessors (`lib.rs:135–337`) and 8 composite transactional ops (`lib.rs:171–363`); 3 migrations; `WAL` + `foreign_keys=ON` verified by read-back (`connection.rs:169–192`); optimistic concurrency via `state_version` | `director-store` integration: **69 tests passing** (executed). `PHASE5-STORE.md` claims 272 workspace tests at the time | **Complete** | Self-reported & fixed: (a) the two partial unique indexes were **inert** because status columns hold JSON-quoted strings and the predicate compared bare literals — "The index existed, was documented as the load-bearing invariant, and enforced nothing at all"; (b) `assign_task` left the departing agent still holding the task. Outstanding: `#![allow(dead_code)]` at `lib.rs:59` for 5–6 written-but-unreached helpers |
| **6** | The Control Loop | Implement `OBSERVE → PLAN → ASSIGN → MONITOR → VERIFY → REPLAN` one step at a time, each landed and tested before the next | `crates/director-app/src/{observe,plan,assign,monitor,verify,replan}.rs` (3,832 lines); `director-domain/src/graph.rs` (Kahn topological sort + cycle/self/unknown-dep validation); atomic plan activation (`store.create_active_plan`) | `director-app` per-step suites: observe **13**, plan **2**, assign **15**, monitor **19**, replan **21**, verify **13**, engine-inline **134** — all passing (executed) | **Complete (as a library of steps)** | **The single most important limitation, stated by the project's own doc:** *"Not an MCP server, and not a binary… nothing yet **drives** them — no process runs the loop unattended and no caller exists outside the tests. Until then, the tests are the caller."* `PHASE6-APP.md` also has **no checked "Definition of done" list** — acceptance is prose plus a test narrative, structurally weaker than Phases 0/1/2/3/5/10/11/12 |
| **7** | Intelligent Scheduler | Remove "the one judgment that does not need a human: *which* ready task goes to *which* available agent" | `director-app/src/schedule.rs` (613 lines) — 4-tier policy; `schedule.rs` adds **no new store method and no new domain entity** | `tests/schedule.rs` **13 passing** (executed) | **Complete** | Doc-acknowledged: one precondition no loop step produces yet — "a task back to `todo` whose former holder is still `Available`" — is reachable in tests only by setting store state directly. Task **priority is deliberately not a scheduling axis** (a design choice worth challenging; see §7.2) |
| **8** | *(no document)* | Promised once as "the pool that manages several identities" | **Never delivered.** | None | **Dangling promise** — see §3.3 | Promised in `docs/PHASE2-ADAPTERS.md:120` ("The pool that manages several identities is **Phase 8**") and in 4+ source comments (`handoff/adapter.rs:409`, `handoff/transport.rs:17`, `domain/agent.rs:62`, `domain/capability.rs:4`, `domain/handoff.rs:1`, `adapters/memory.rs:578`). `git log -S "Phase 8"` shows the label was written once in Phase 2 and never revisited. Consequence: **one adapter instance speaks as one agent**, so Orqynn cannot yet present a multi-agent pool to the substrate |
| **9** | *(no document)* | RecentContext engine (a bounded, compacted window of recent activity) | **Never delivered.** | None | **Dangling promise** — see §3.3 | Aspirational label in `docs/PHASE0-FORENSICS.md:660` ("Control flow (Phase 9 onwards)") and one commit (`2502a92`). `domain/context.rs` exists with `RecentContext`, `DEFAULT_MAX_ACTIONS: usize = 50`, `ContextSnapshot` — but it is **in-memory only, never persisted and never called by the loop**. `PHASE12` confirms: "`RecentContext` has no table and no repository" |
| **10** | Verification Engine | Close the gap that VERIFY produced a verdict with **no durable record**: the code that resolves criteria into probes, gathers evidence, applies precedence, builds the record and writes it | `director-app/src/engine.rs` (736 lines) + `verify.rs` (760 lines); `domain/src/verification.rs` (506 lines); `0003_verifications.sql`; `store/src/verifications.rs` (627 lines); `Store::apply_verification` (`lib.rs:363`) | `tests/verify.rs` **13**, `engine` inline **134** of 619 total; store verification tests green | **Complete** | Doc-acknowledged: **only `Probe::Command` is implemented.** `TestSuite`, `Diff` and `File` are modelled in the domain and answered as `Unverifiable` by `run_probe`. "Judging prose is a later phase's work" |
| **11** | Runtime & Recovery Validation | *Add nothing.* Prove the property the architecture exists for: observe→…→replan→continue **without losing persistent project state** | `tests/runtime_recovery.rs` (1,212 lines) — 5 named integration tests + a failure matrix (A–L, every row "no" for data loss and "no" for false completion); a **direct-completion audit** tracing every `UPDATE tasks SET status` write site | Doc claims **537 passed / 0 failed / 13 ignored** at this point. **Re-run today: 608 / 0 / 13 — strictly better, consistent.** | **Complete** | Doc-acknowledged limitations: (a) **"No process boundary, so no true kill test"** — crash survival is simulated in-process, not by killing a real daemon; (b) `Blocker` is **not persisted at all** (no table, no migration, no repository); (c) `ProviderSync` is never written; (d) `OBSERVE` takes a concrete `GitService` rather than coding against the `ProjectStateProvider` trait — the one step the loop does not reach through a trait; (e) MCP failure behaviour is tested at the adapter, never inside the loop, so an MCP failure cannot affect orchestration state today. Also self-reported: the baseline run failed 8 tests from 111 accumulated shared temp dirs (fixed in `fd672a4`), and the new test file shipped with 16 `fmt` drift sites + 1 clippy error, meaning "`fmt --check` and `clippy` were in fact failing on the tree this document describes as clean" |
| **12** | Checkpointing & Recovery Context | (1) `director-app::checkpoint` writes checkpoints automatically inside the observation round; (2) `director-app::recovery` assembles the picture in one place, and SCHEDULE hands a replacement its briefing | `director-app/src/checkpoint.rs` (1,304 lines) + `recovery.rs` (976 lines); `TaskRepository::create_checkpoint` gains `expected_current_version: Option<u64>` (the optimistic guard); `recovery::RecoveryBuilder`; `schedule` attaches `Option<Recovery>` to `Scheduled`; `Store::checkpoints()`; lineage-derived ids `CHK-<task>-N` | `tests/checkpoint_lineage.rs` **1**, `tests/recovery_builder.rs` **18**, `tests/end_to_end.rs` **2**, `runtime_recovery.rs` **5** — all passing (executed) | **Complete** | Doc-acknowledged: "**It does not add a process.** There is still no daemon, no driver, and no MCP server." / "does not persist `RecentContext` or `Blocker`" / "does not resume an agent automatically". **Consequence:** `RecoveryGaps::structural()` (`recovery.rs:160`) sets *both* gap flags unconditionally, so `Recovery::is_clean()` is `false` and `actions_available()` is `false` **for every build today**. Documentation defect: `PHASE12`'s "The finished edit" section contains a duplicated/garbled paragraph (lines 208–212) |
| **13** | Deterministic Conflict Detection | Use task `scope_paths` so two agents told to touch disjoint trees cannot collide on the same files — collisions *detected*, not hoped away | **Not implemented.** The *ingredients* exist: `Task.scope_paths: Vec<String>` (`domain/task.rs:205`, persisted as JSON in `tasks.scope_paths`) and `Task::overlaps_scope(&self, other: &Task) -> bool` (`domain/task.rs:296`), with unit tests at `task.rs:422–434`. But **`overlaps_scope` has zero call sites outside its own tests**, and neither SCHEDULE nor ASSIGN consults it. `GitObserver::has_conflicts` (`observer.rs:691`) only reports *git-level* merge conflict markers, which is a different problem | README marks Phase 13 "🔄 Planned"; `task.rs:204` comment says "(Phase 13)" | **Planned** | Entire feature outstanding. Also unnumbered: task **decomposition** (PLAN decides *what* the tasks are; today the caller supplies every `TaskSpec`), and the **blocker lifecycle** — both explicitly de-numbered in `1a166df` |

### 3.3 Phase-numbering gaps — explanation, not invention

| Number | Status | Evidence |
|---:|---|---|
| **4** | **Folded into Phase 5, then mis-documented as "deliberately skipped"** | Code evidence is decisive: `git log -S "Phase 4" -- crates` → exactly two commits. `f80184e`, labelled *"Phase 5 (complete): director-store"*, **created** `director-domain/src/checkpoint.rs`, `director-domain/src/store.rs` and `director-store/src/checkpoints.rs`, all carrying "Phase 4" doc comments (`store.rs:17` "that is the acceptance property in the Phase 4 specification"; `store.rs:350` "Phase 4 stored the durable foundation"; `checkpoint.rs:105` "a payload written before Phase 4 predates the notion of supersession", with test `a_pre_phase4_checkpoint_payload_still_deserializes`). Phase 12's commit message confirms the lineage: "the **Phase 12 activation of the Phase 4 foundation**". Three documents nevertheless claim Phase 4 never existed. **Those documents are wrong.** |
| **8** | **Promised, never delivered, never renumbered, never acknowledged** | One doc mention (`PHASE2-ADAPTERS.md:120`) and 6 source comments. All traceable to Phase-2-era commits only. Not listed as remaining work in either AUDIT. **This is a genuine unfulfilled promise**, and it blocks the multi-agent pool story. |
| **9** | **Aspirational label only; never built, never renumbered** | `PHASE0-FORENSICS.md:660` + one commit (`2502a92`). `context.rs` exists but is unpersisted and uncalled. Only *partially* acknowledged, in `PHASE12`'s limitations. |

**Verified facts** = the commit table, the `git log -S` results, the source/test file inventory, and the executed gate results. **Prior-audit claims** = the test counts (174 / 190 / 209 / 272 / 299 / 310 / 312 / 471 / 494 / 507 / 537) and the phase-completion table in `docs/AUDIT-2026-09-30.md` and `docs/AUDIT-2026-10-04.md`. **Reasonable inference** = that Phases 4/8/9 were renumbered into later work rather than abandoned; the inference for 4 is now *proven*, for 8 and 9 it remains inference.

### 3.4 Prior-audit claims vs. reality

| Prior claim | Source | Reality (verified today) |
|---|---|---|
| "608 tests passing" (README badge) | `README.md:10` | ✅ **Exactly correct.** 606 unit/integration + 2 doctests = 608 |
| "499/310 vs 299 passed" | `AUDIT-2026-09-30.md` | Unreconciled **internal** contradiction in that document (310 in its resolution note vs 299 in §4). Historical; superseded |
| "494 passed / 507 total" | `AUDIT-2026-10-04.md` | Historical snapshot; superseded by 608 |
| "Phase 7 and the Phase 10 record layer are uncommitted/unpushed" | `AUDIT-2026-10-04.md` F1/F2 | ✅ **Resolved** — both are ancestors of `origin/main` (`2e1f4fa`, `ad9dead`) |
| F1: `plan.rs` (610 lines) was never compiled — no `pub mod plan;` | `AUDIT-2026-09-30.md` | ✅ **Resolved** — `lib.rs` declares it; the fix is the `wiring.rs` regression test (`cf2231e`) that fails the build if a source file is not in the module tree |
| F4: "a stale duplicate checkout exists beside the project: `../orqyn/` … Recommendation: delete it" | `AUDIT-2026-10-04.md` | ⚠️ **Roles have swapped.** That audit called `../orqyn/` the stale copy; today `orqyn/` is canonical/current and `director-brain/` is the stale one. Any plan quoting the old naming verbatim will target the wrong directory |
| "No design defects and no correctness bugs in committed code" | `AUDIT-2026-09-30.md` | Consistent with today's gates: 608/0, fmt clean, clippy `-D warnings` clean. **But** the sample is narrow — it was a code read, and the two live MCP suites never ran in this audit |
| Prior audit's Phase 4/8/9 conclusions | untracked `ORQYNN_PROJECT_AUDIT.md` | **Contradicted** by the code evidence in §3.3 (it treated Phase 4 as non-existent) |
| "previously reported end-to-end failures" | prior session (not present in repo) | ❗ **NOT VERIFIABLE.** No such record exists in the repository. The only end-to-end artifact present is `tests/end_to_end.rs` (**2 tests, both passing, executed**) and an untracked `end_to_end_output.txt` log. **No end-to-end failure was reproduced.** The known real-world E2E gap is Phase 11's own admission: *"No process boundary, so no true kill test."* |

---

## 4. Architecture & Module Responsibilities

### 4.1 The problem and the concept

Modern coding agents are good at local code generation and bad at *coordination*. Orqynn's thesis is that the missing layer is an **objective, supervisory brain** that sits *above* the agents and *beside* the persistence substrates. Two design commitments follow, and they are what make the codebase interesting:

**(a) Orchestration is separated from agent runtimes.** Orqynn never spawns, embeds or links a coding agent. Agents are harness *labels* (`Harness::ClaudeCode`, `Codex`, `Goose`, …) plus capability sets plus heartbeats. This keeps Orqynn vendor-independent: swapping Claude Code for a local model is a registry row.

**(b) Orchestration is separated from external substrates.** Orqynn never imports `handoff-mcp` or `ai-memory` source or internal data structures. It speaks **MCP JSON-RPC 2.0 over stdio**, line-delimited, via `stdio_mcp.rs`. This is not a convention — `crates/director-adapters/tests/boundary.rs` **scans the workspace and fails the build** if any crate outside `director-adapters` names a substrate identifier or depends on it. The boundary is a test.

### 4.2 The four-crate architecture

```
director-domain      pure core: entities, 16 sealed newtype IDs, state machines,
                     Kahn topological sort, 7 provider traits, 11 store traits.
                     Zero substrate coupling. #![forbid(unsafe_code)]
        ▲                    ▲                    ▲
        │                    │                    │
director-adapters    hexagonal adapters: InMemoryProvider, LocalExecutor,
        │             GitObserver/Service/Store (git2 0.21 read-only),
        │             HandoffAdapter, AiMemoryAdapter, StdioMcpTransport.
        │             The ONLY crate permitted to name a substrate.
        │
director-store       embedded SQLite: 3 migrations, 15 tables, WAL + FKs,
        │             optimistic concurrency (state_version), 3 partial-unique
        │             invariant guards, 8 composite transactional operations.
        │
director-app         the control loop as composable async functions:
                     observe, checkpoint_round, plan, schedule, assign, monitor,
                     verify/engine, replan, recovery::RecoveryBuilder.
                     #![forbid(unsafe_code)] via [lints.rust].
```

Dependency direction is strictly inward: `app → store → domain`, `app → adapters → domain`. `adapters` does **not** depend on `store`; `store` does **not** depend on `adapters`. This is verifiable and was not violated.

### 4.3 The control loop: `OBSERVE → PLAN → SCHEDULE → ASSIGN → MONITOR → VERIFY → REPLAN`

| Step | Entry point | Responsibility | Store operations |
|---|---|---|---|
| **OBSERVE** | `observe::observe` / `observe_at` (`observe.rs:141/167`) | Read the git working tree through `git2`; produce `ProjectStateSnapshot`; detect tree changes, renames, deletions; sync normalized state into the store | `project_state`, `provider_sync` |
| **CHECKPOINT** (inside OBSERVE) | `checkpoint::checkpoint_round` (`checkpoint.rs:436`) | For every `in_progress` task whose round recorded a **real** change (moved HEAD, moved branch, or changed working tree), derive and persist a `Checkpoint`. Quiet rounds write nothing; a checkpoint that merely restates the current one is detected (`restates_current`, `:255`) and skipped. Per-task conflicts are collected and the round continues (`CheckpointConflict` + `retry_against`) | `checkpoints` (+ best-effort `MemoryProvider` publish, 10 s `MEMORY_PUBLISH_TIMEOUT`) |
| **PLAN** | `plan::plan` / `plan_at` (`plan.rs:188/195`) | Validate `TaskSpec`s against DAG rules (self-dependency, unknown dependency, cycle); compute a deterministic topological order; **atomically** create the plan, its tasks and its activation in one transaction | `Store::create_active_plan` |
| **SCHEDULE** | `schedule::schedule` / `schedule_at` (`schedule.rs:130/141`) | Match ready tasks to eligible agents via the 4-tier policy (§4.4). Before the write, build the recovery context for tasks that are being re-handed to a replacement, and attach it | reads only — *the round writes nothing itself*; ASSIGN owns the write |
| **ASSIGN** | `assign::assign` / `assign_at` (`assign.rs:107/114`) | Acquire a claim-once lease atomically; move the task to `in_progress`; record the assignment; validate legality (plan active, task in plan, deps done, agent has capabilities) | `Store::assign_task` / `assign_and_start` |
| **MONITOR** | `monitor::monitor` / `monitor_at` (`monitor.rs:169/176`), `acknowledge`, `report_done` | Survey in-flight work; compare heartbeat age against TTL; expire dead leases (session → `vanished`, task → `todo`); acknowledge progress; move finished work to `verification_pending` | `Store::expire_lease`, `acknowledge_assignment`, `report_completion` |
| **VERIFY** | `verify::verify` (`verify.rs:266`) and `engine::verify_task` (`engine.rs:115`) | Resolve checkable criteria into probes; execute via `ExecutionProvider`; read **raw exit codes**; apply precedence (Failed > Unverifiable > Passed, Observed never decides); write an append-only `Verification` | `Store::apply_verification` |
| **REPLAN** | `replan::replan` / `replan_at` (`replan.rs:379/384`) | Survey failed / stranded work; apply caller-specified remediations (`Retry`, `Rework{contract}`, `Cancel{reason}`); report dependents left stranded without writing them | plans, decisions, tasks |
| **RECOVERY** (on demand) | `RecoveryBuilder` (`recovery.rs:313`), `recovery_context_from_store` (`:545`) | Assemble the continuation package — task contract, version history, checkpoint lineage, prior verdicts, git comparison — and derive `resume_status` + `next_action` | reads |
| **STEP ENGINE** | `director-app/src/engine.rs` | **Despite the name, this is not a loop driver.** It is the *verification engine*: `probes_for`, `run_probe` (the single extension point), `judge`, `destination` | `Store::apply_verification` |

⚠️ **There is no loop driver.** Grep for `run_loop`, `daemon`, `#[tokio::main]`, `fn main`, `clap`, `interval(` across `src/` returns **zero** production hits. The only `loop {}` statements are a `try_wait` poll in `executor.rs:86` and a JSON-RPC read loop in `stdio_mcp.rs:232`. `director-app/src/lib.rs:39–44` states it plainly: *"Not an MCP server, and not a binary yet … what is missing is a driver. Until one exists, the tests are the caller."*

### 4.4 Scheduling policy, leases, optimistic concurrency, failure recovery

**Scheduling — 4 tiers** (`schedule.rs:141–224`, pure ranking extracted to `match_agents`, `:243–270`):

1. **Eligibility** — the agent must be available and hold all `required_capabilities`. Capability matching is hierarchical (`Capability::matches`, `domain/capability.rs:64`).
2. **Fresh eyes first** — agents with **fewer prior tenures** on this task rank higher. This is the anti-bias rule: an agent that already failed the task is deprioritised.
3. **Specialist first** — an agent whose capability set is a *narrower* match (a specialist) beats a generalist, conserving generalists for work nobody else can do.
4. **Deterministic tie-break** — by `AgentId`, so two identical runs produce identical assignments.

Deliberate non-features, stated in code: **task priority is not a scheduling axis**; **one task per agent per round** (a `committed: HashSet<AgentId>` prevents double-booking); the schedule round is a **snapshot/proposal**, and ASSIGN re-validates live before writing.

**Leases.** An `AgentAssignment` is a first-class, time-boxed tenure with `ReleaseReason`, `activated_at`/`released_at`, and its own `state_version`. `MONITOR` expires a lease whose heartbeat has aged past the TTL: the session is marked `vanished`, the disconnect is recorded, and the task returns to `todo` for immediate reassignment. Crucially, **the task's identity and version history are immutable across reassignment** — a replacement gets a *new* assignment row, not a mutated one.

**Optimistic concurrency.** Every mutable table carries `state_version INTEGER NOT NULL DEFAULT 1`. Writes are `UPDATE … SET state_version = state_version + 1 WHERE id = ? AND state_version = ?`; zero rows affected becomes `StoreError::StateVersionConflict { entity, id, stored, caller }`. `connection.rs:224` documents the deliberate trade-off: `stored` is *inferred* (`caller + 1`) rather than re-read. `PHASE11` proves two concurrent writers produce exactly one winner and one structured conflict.

**Failure recovery.** Four distinct recovery paths, all implemented:
- **Expired lease** → `expire_lease` → session `vanished`, task → `todo`.
- **Orphaned task** (in progress, no live holder) → surfaced by MONITOR as `InFlight::Orphaned`, repaired by REPLAN.
- **Failed verification** → REPLAN applies `Retry` / `Rework{amended contract}` / `Cancel{reason}` and reports the dependency cascade **without writing it**.
- **Agent crash mid-work** → OBSERVE's checkpoint writer has already persisted an evidence-backed checkpoint; SCHEDULE's `RecoveryBuilder` reassembles the continuation package and attaches it to the assignment.

**Conflict detection — what exists and what does not.**
*Exists:* optimistic-concurrency conflicts (store), git merge-conflict-marker detection (`GitObserver::has_conflicts`), checkpoint-lineage conflicts (`CheckpointConflict`), agent-busy refusals, and capability-eligibility refusals.
*Does **not** exist:* **deterministic file-level conflict detection.** `Task::scope_paths` and `Task::overlaps_scope` exist and are tested in isolation, but `overlaps_scope` is **never called by the loop**. Two agents scheduled onto tasks with overlapping `scope_paths` will both be assigned and will edit the same files — with no detection and no guard. This is Phase 13, and it is **not implemented**.

### 4.5 Persistence, checkpoint lineage, continuation context, restart behaviour

**SQLite configuration** (`connection.rs:169–192`): `journal_mode=WAL` — and the adapter **reads the value back and refuses to proceed if it is not `wal`**; `foreign_keys=ON`; `synchronous=NORMAL` (a documented durability trade-off); `busy_timeout=5000 ms`; pool size 8.

**Schema: 15 tables + `schema_migrations`.** `projects`, `repositories`, `tasks`, `task_dependencies`, `task_status_history`, `agents`, `agent_sessions`, `agent_assignments`, `checkpoints`, `project_states`, `provider_sync` (0001); `plans`, `decisions` (0002); `verifications` (0003).

**26 index statements, of which 3 are load-bearing partial-unique invariants:**
- `idx_assignments_active_per_task ON agent_assignments(task_id) WHERE status = 'active'` — at most one active holder per task
- `idx_checkpoints_current_per_task ON checkpoints(task_id) WHERE status = 'current'` — at most one current checkpoint per task
- `idx_plans_active_per_project ON plans(project_id) WHERE status = 'active'` — at most one authoritative plan per project

⚠️ These predicates compare **JSON-quoted** status strings, not bare literals. Phase 5 shipped them as bare literals and they **silently enforced nothing** — the single most instructive defect in the project's history, and it is documented in `AUDIT-2026-09-30.md` and re-explained at the call sites.

**Checkpoint lineage.** `CheckpointStatus::{Current, Superseded}`; creating a new current checkpoint supersedes the previous one **without deleting it**, and `latest_checkpoint(task)` is a partial-unique-indexed query. **Ids advance with the lineage, not with a process-local counter** (`CHK-<task>-N`). Phase 12 documents exactly why: the previous implementation re-minted `CHK-work-1` for a task that already had it, and the whole observation round aborted on a primary-key collision. `create_checkpoint` takes `expected_current_version: Option<u64>` — `None` means "there must be no current checkpoint" — so a stale writer gets `StateVersionConflict` and supersedes nothing, and `CheckpointRound` collects that per task and keeps going rather than losing every other task's checkpoint.

**Continuation context.** `ContinuationPackage` (15 fields) carries the task contract, version history, checkpoint lineage, prior verdicts and a git comparison. `Checkpoint::compare_with` classifies the observation as `Ready` / `Stale` / `Conflicted` / `Blocked` / `Insufficient`, and `next_action_for` derives the literal next instruction. `RecoveryOutcome` distinguishes `Built`, `NoCheckpoint`, `UnknownTask` and `Gap`.

**Restart behaviour.** Id minting is **caller-supplied** (`&mut IdGenerator` threaded through `observe`, `schedule`, `plan`), checkpoint ids are **lineage-derived**, and the migration runner **refuses to open a database newer than the build** rather than corrupting it. Migrations are embedded with `include_str!`, so a listed-but-missing or unlisted-but-present file fails the build; versions must be contiguous (asserted by a test).

### 4.6 Verification guarantees — how unverified agent claims are prevented

This is the strongest part of the design and the claim holds up under inspection:

1. **Type-level.** `Task` has **no agent field** (`domain/task.rs`, verified by absence). `TaskId` and `AgentId` are distinct sealed newtypes that cannot be coerced into one another. Work identity outlives worker identity.
2. **State-machine level.** `TaskStatus::reachable_by_agent_report()` (`task.rs:149`) **excludes `Done`**. An agent reporting completion moves a task to `VerificationPending` — never `Done`.
3. **Provider-trait level.** None of the 7 provider traits can complete a task. `HandoffAdapter` defends this at the boundary: `task_from_wire(data, trusted_done_ids)` (`handoff/mapping.rs:81`) takes an explicit set of externally-trusted done ids, so a substrate that self-reports `done` is not believed.
4. **Write-path level.** `PHASE11`'s direct-completion audit traces every `UPDATE tasks SET status` write site and concludes `Store::apply_verification` is **the only path to `Done`**.
5. **Evidence level.** `CommandOutcome::succeeded()` is `exit_code == 0 && !timed_out` — raw exit codes, read by Orqyn itself, never by the agent. Verdicts are recorded **append-only**; re-judging accumulates history rather than overwriting.
6. **Precedence level.** `Failed` decides first, `Unverifiable` second, `Passed` third, `Observed` never decides. **A check that cannot run is a property of the environment, not a verdict on someone's work** — the single most defensible design decision in the codebase.

**What is *simulated* rather than real:** crash survival (no process boundary, so no `kill -9`); rebase tolerance (synthetic git fixtures, not concurrent real rebases); MCP failure behaviour (tested at the adapter, never inside the loop, so **an MCP failure cannot affect orchestration state today**); and `Probe::{TestSuite, Diff, File}` — modelled, never executed, always answered `Unverifiable`.

### 4.7 Adapter & external MCP substrate integration

`StdioMcpTransport` (`stdio_mcp.rs`) spawns a substrate binary with `tokio::process::Command`, applies `env_remove` **before** `extra_env` (ordering is deliberate and commented), pipes stdin/stdout, **nulls stderr**, and performs the `initialize` → `notifications/initialized` handshake. Its `Debug` impl is **redacted to expose only `next_id`**, so no command line or environment can leak into logs — a genuinely careful touch. It implements `Drop` for shutdown.

Two generic adapters (`HandoffAdapter<T: HandoffWire>`, `AiMemoryAdapter<T: AiMemoryWire>`) each have **three** wire implementors: the real transport, a `Mutex<Fake…>` for tests, and a `Mutex<Broken>` for failure-path tests. The `HandoffWire` / `AiMemoryWire` trait seam is what makes the substrate testable at all.

**Live status:** the 13 live tests (8 ai-memory, 5 handoff) are `#[ignore]`d because they spawn real server binaries. **They were not run in this audit** and are **NOT VERIFIED** in this report. Per `docs/PHASE3-MEMORY.md`, they last passed against `handoff-mcp v0.35.1`.

### 4.8 Implemented vs. experimental vs. simulated vs. planned

| Category | Members |
|---|---|
| **Implemented & tested** | Domain model; DAG validation + topological order; SQLite store with migrations, optimistic concurrency, 3 partial-unique invariants, 8 transactional ops; OBSERVE; CHECKPOINT (inside OBSERVE); PLAN; SCHEDULE (4-tier); ASSIGN; MONITOR; VERIFY + `Probe::Command`; REPLAN; `RecoveryBuilder`; git observation (git2); HandoffAdapter; AiMemoryAdapter; StdioMcpTransport; LocalExecutor; InMemoryProvider |
| **Implemented but limited** | `observe::test_results` is a **stub** — it ignores its argument and returns `None` (`observe.rs:295`); `Recovery::actions_available()` is **always `false`** because `RecoveryGaps::structural()` sets both flags unconditionally (`recovery.rs:160`) |
| **Modelled but never executed** | `Probe::{TestSuite, Diff, File}` — always `Unverifiable`; `Blocker` and `Handoff` domain types — `Blocker` has **no table, no migration, no repository**; `RecentContext` — in-memory, unpersisted, uncalled |
| **Simulated in tests only** | Crash survival; concurrent writers (real threads, in-process); multi-agent pool (single adapter = single agent identity) |
| **Planned** | Phase 13 conflict detection; PLAN decomposition; blocker lifecycle; multi-identity pool (dangling Phase 8); RecentContext persistence (dangling Phase 9); any driver/CLI/daemon/MCP server |

---

## 5. Current Public APIs & How to Actually Run or Test the Project

**There is no way to *run* Orqynn. There is a way to *test* it.**

| Surface | Reality |
|---|---|
| Binary / executable | **None.** No `[[bin]]` in any `Cargo.toml`, no `src/main.rs` anywhere |
| CLI | **None.** No `clap`, `structopt`, or argument parsing in any dependency |
| Daemon / supervisor | **None.** No `run_loop`, no `#[tokio::main]`, no interval scheduler |
| Orqynn MCP server | **None.** Orqynn is an MCP *client* only. It serves no tools |
| `examples/` | **None** |
| `benches/` | **None** |
| Library crates | 4, all `0.1.0`, all importable |

**Public entry points an integrator would call** (all `pub`, all in `director-app` unless noted):

```
director_app::observe::observe        (&Store, &GitService, &mut IdGenerator, &impl MemoryProvider,
                                      &ProjectId, &RepositoryId, &str) -> Result<Observed, ObserveError>
director_app::checkpoint::checkpoint_round(&Store, &mut IdGenerator, &impl MemoryProvider,
                                      &ProjectId, &StoredProjectState, &SyncResult,
                                      &WorktreeState, DateTime<Utc>) -> Result<CheckpointRound, CheckpointError>
director_app::plan::plan              (&Store, PlanRequest) -> Result<Planned, PlanError>
director_app::schedule::schedule      (&Store, &mut IdGenerator, &ProjectId) -> Result<ScheduleReport, ScheduleError>
director_app::assign::assign          (&Store, AssignRequest) -> Result<Assigned, AssignError>
director_app::monitor::monitor        (&Store, &ProjectId) -> Result<MonitorReport, MonitorError>
director_app::verify::verify          (&Store, &impl ExecutionProvider, &ProjectId, &Path) -> Result<VerifyReport, VerifyError>
director_app::replan::replan          (&Store, ReplanRequest) -> Result<ReplanReport, ReplanError>
director_app::recovery::RecoveryBuilder::new(&Store, TaskId).from_stored_state().build() -> Result<RecoveryOutcome, _>
director_store::Store::open           (impl AsRef<Path>) -> Result<Store, StoreError>
director_adapters::{GitService, LocalExecutor, InMemoryProvider, HandoffAdapter, AiMemoryAdapter}
```

Note the **seam asymmetry** that Phase 11 flagged: OBSERVE takes a concrete `GitService`, not `&impl ProjectStateProvider`. It is the one step that does not reach its substrate through a trait.

**Can an external developer use it without writing internal test code?** **No.** They can add `crates/*/tests/*.rs` (the API is genuinely public and well-shaped — the README example compiles as `tests/readme_example.rs`), but there is nothing to invoke. The `README.md` "Code Example" is honest about this and pins the API with a compiled test.

---

## 6. Codebase Maturity & Validation Results

### 6.1 Executed gates (this audit, 2026-10-16, on `65cfa02`)

| Gate | Command | Result |
|---|---|---|
| Build + test | `cargo test --workspace --all-targets -- --test-threads=4` | ✅ **21 test binaries, 606 passed, 0 failed, 13 ignored** |
| Doctests | `cargo test --workspace --doc` | ✅ **2 passed, 0 failed** (`ids.rs:244`, `store/lib.rs:30` compile-only) |
| **Total** | | ✅ **608 passed / 0 failed / 13 ignored** — matches the README badge exactly |
| Format | `cargo fmt --all --check` | ✅ exit 0, no diff |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` | ✅ exit 0, zero warnings |
| Live MCP suites | `cargo test --workspace -- --ignored` | ⚠️ **NOT RUN** — requires `handoff-mcp` and `ai-memory` server binaries, which are not present in this environment |

**Per-suite breakdown (executed):** adapters unit 77 · domain unit 168 · store unit 29 · store integration 69 · app `assign` 15 · app `engine` 134 · app `checkpoint` (inline) 13 · app `monitor` 19 · app `observe` 13 · app `schedule` 13 · app `replan` 21 · app `verify` 13 · app `runtime_recovery` 5 · app `recovery_builder` 18 · app `checkpoint_lineage` 1 · app `end_to_end` 2 · app `readme_example` 1 · adapters `wiring` 3 · adapters `boundary` 1 · doctests 2. *(Groupings are approximate where the inline unit suite is reported as one binary; the authoritative total is 606 + 2.)*

**Total wall time for the full suite: well under a minute.** This is a cheap, fast project to validate — a genuine asset.

### 6.2 Previously reported end-to-end failures — investigated

No record of any end-to-end failure exists inside the repository. Specifically investigated:

- `crates/director-app/tests/end_to_end.rs` — **exists, 2 tests, both passing** (executed).
- `docs/PHASE11-RUNTIME-RECOVERY.md` — the failure matrix (A–L) is documented as **all "no" for data loss and all "no" for false completion**.
- `docs/AUDIT-2026-10-04.md` §5 — described the Phase 10 record layer as "uncommitted … one disk event from being lost". ✅ **Resolved** by `ad9dead`; the layer is committed and its 69 store-integration tests pass.
- `docs/AUDIT-2026-09-30.md` F1 — `plan.rs` "has **never been compiled**. Its 10 tests have **never run**." ✅ **Resolved** by `cf2231e`, which added `tests/wiring.rs` (a build-failing regression test for exactly this class of defect). `plan` now runs.
- Untracked `end_to_end_output.txt` — a scratch log, untracked, not part of the audited history.

**Conclusion:** no reproducible end-to-end failure exists in the current tree. The *structural* E2E gap is Phase 11's own admission — *"No process boundary, so no true kill test"* — which is a **roadmap item (R2)**, not a defect.

### 6.3 Assessment against each dimension

| Dimension | Rating | Evidence |
|---|---|---|
| **Architecture & modularity** | **Excellent** | Strictly inward dependencies; the substrate boundary is a build-failing test (`tests/boundary.rs`); a second build-failing test (`tests/wiring.rs`) prevents the "file never compiled" class of defect from recurring |
| **Code organisation** | **Good** | 63 focused source files, cohesive modules, domain/store/app separation respected. **Deductions:** `director-domain/src/repository.rs` is 1,261 lines covering five distinct responsibilities; `handoff/adapter.rs` is 1,294 lines; `crates/director-store/src/tests/store.rs` is a single 3,272-line integration file |
| **Core orchestration completeness** | **Complete as a library; incomplete as a system** | All 7 steps exist as composable functions with real persistence. **No driver assembles them into a loop** |
| **Persistence & data integrity** | **Excellent** | WAL + FK verified by read-back; 3 partial-unique invariants; append-only verifications/plans/decisions; optimistic concurrency proven by a two-writer test. **Deductions:** `#![allow(dead_code)]` at `store/src/lib.rs:59`; 6 helpers written but unreached; `Blocker` and `RecentContext` have no persistence at all |
| **Concurrency & deterministic conflict handling** | **Good (store) / Absent (files)** | Optimistic concurrency and deterministic tie-breaking are solid and tested. **Deterministic file-level conflict detection does not exist** — `overlaps_scope` is dead code outside its own tests |
| **Checkpointing & recovery** | **Strong** | Evidence-backed checkpoints, lineage-derived ids, supersede-not-delete, per-task conflict tolerance, `RecoveryBuilder` assembled in one place and attached by SCHEDULE. **Deductions:** `actions_available()` always `false`; `RecentContext`/`Blocker` gaps make every recovery report "unclean" |
| **Verification & replanning** | **Strong** | `Done` is unreachable by agent report at four independent layers; raw exit codes; append-only verdicts; correct Failed > Unverifiable > Passed precedence. **Deductions:** 3 of 4 `Probe` kinds unimplemented; `observe::test_results` is a stub |
| **Test quality** | **Excellent** | 619 tests / 619 meaningful assertions; behaviour-descriptive names (`a_failing_dependency_does_not_unblock_the_task`); negative tests (`a_missing_task_is_none_not_an_error`, `observing_an_unseeded_path_is_an_error_not_invention`); boundary and wiring regression tests. **Deductions:** no property tests, no fuzzing, no concurrency-stress beyond one two-writer test, no coverage tooling, 13 live tests unexercised in this audit, and `observer.rs` (639 lines, security-relevant path validation) still has **no tests of its own** |
| **Error handling & observability** | **Good, with a gap** | Typed `thiserror` enums per step; structured `StoreError::StateVersionConflict`; `CheckResult::Failed` carries exit code, timed-out flag, stdout and stderr; transport `Debug` redacted. **Deduction:** `tracing` is a declared dependency and used in `director-store`, but **there are effectively no logs, no metrics, no traces, and no correlation IDs** — the crate's own docs are the observability story |
| **Security** | **Good foundations** | `#![forbid(unsafe_code)]` on domain/adapters/app; **0 `unsafe` blocks**; **0 secrets/credentials/API keys** in source; path traversal defence in `Repository::validate_path` (`repository.rs:877`, rejecting `..` and rooted-elsewhere paths); stderr nulled; `Debug` redacted. **Live risks:** `LocalExecutor` spawns arbitrary commands from task-defined criteria via `std::process::Command` (by design, but it is a command-injection surface that will need an allowlist policy before production); `LocalExecutor` is blocking and is **not** wrapped in `spawn_blocking` (self-documented at `executor.rs:10–14`); ~55 `unwrap()`/`expect()` in production `src/` outside `#[cfg(test)]` |
| **Documentation & DX** | **Excellent, with stale spots** | 13 phase docs, 3,585 lines, with acceptance criteria and *self-declared* limitations. **Deductions:** Phase 6 has no Definition-of-Done checklist; Phase 4's history is documented **incorrectly** in three places; Phase 12's "finished edit" section contains a garbled duplicated paragraph; the README badge path `docs/assets/banner.png` **does resolve** (correct), but the README's "608 Passed" badge links to `tests/` which is not a directory at the repo root |
| **Build reproducibility & CI** | **Weak** | `rust-toolchain.toml` pinned; `Cargo.lock` committed; `.gitattributes` forces LF; release profile tuned. **But: `.github/` does not exist — zero CI.** No MSRV matrix, no Linux/macOS coverage, no `cargo audit`, no `cargo deny`, no dependency-update bot, no release automation. The Windows toolchain pin (`stable-x86_64-pc-windows-gnu`) is not obviously portable |
| **Runtime usability & deployment readiness** | **Not ready** | No artifact of any kind (§5) |

### 6.4 Small measured defects worth fixing (all verified)

1. `README.md:10` — the "Tests 608 Passed" badge links to `tests/`, which does not exist at the repo root (crates own the test dirs).
2. `docs/PHASE5-STORE.md`, `docs/AUDIT-2026-09-30.md`, `docs/AUDIT-2026-10-04.md` — all three assert Phase 4 never existed; the Git history proves it did (§3.3).
3. `crates/director-app/src/schedule.rs.bak` and `.original` sit in `src/` and are **not gitignored**; `.gitignore` covers `**/*.rs.bk` but not `.bak`/`.original`.
4. `Cargo.toml` declares `uuid` and `sha2` in `[workspace.dependencies]`; **no crate uses them** (dead manifest entries).
5. `README.md` quickstart says "Rust 1.85+"; `docs/PHASE0-FORENSICS.md` says "Phase 1 requires installing rustup with toolchain 1.95+". Contradictory prerequisite docs.
6. `rust-toolchain.toml` pins a Windows-GNU triple, which will not resolve on Linux/macOS for a contributor who has not overridden it.

---

## 7. Bugs, Limitations & Security Risks

### 7.1 P0 — must fix before publishing

| ID | Issue | Evidence | Impact |
|---|---|---|---|
| **P0-1** | **No CI whatsoever.** `.github/` does not exist | `Test-Path .github` → False | Nothing verifies a contributor's PR. For an open-source project this is disqualifying |
| **P0-2** | **No runnable artifact.** 0 binaries, 0 examples, 0 CLI, 0 daemon | scan of all `Cargo.toml`; 0 `main.rs` | Nobody can *use* Orqynn. The project is a library with no consumer-facing surface |
| **P0-3** | **Two working copies of the same remote have diverged**, one with a silent data-model regression pending | §2.3 | Merging `director-brain` forward would revert `Checkpoint::supersede()` semantics and the README's Phase 12 claims |
| **P0-4** | **42 untracked files including two `.bak`/`.original` files inside `src/`**, none gitignored | `git status --porcelain`; `.gitignore` | One `git add -A` commits editor backups and stale logs into `src/` |

### 7.2 P1 — correctness and design gaps

| ID | Issue | Evidence | Impact |
|---|---|---|---|
| **P1-1** | **Deterministic file-level conflict detection does not exist.** `Task::overlaps_scope` is implemented and unit-tested but **has no call sites outside its own tests** | `domain/task.rs:296`; grep across `app/` and `store/` returns no caller | Two agents can be assigned overlapping `scope_paths` and silently collide. This is the project's single biggest *functional* gap |
| **P1-2** | **`Blocker` is never persisted** — no table, no migration, no repository, and no loop step raises one | `PHASE11-RUNTIME-RECOVERY.md` limitations; `domain/src/blocker.rs` unreachable | A modelled concept with no runtime effect |
| **P1-3** | **`ProviderSync` is never written** | `PHASE11` limitations | Sync bookkeeping exists in the schema and is dead in practice |
| **P1-4** | **`Recovery::actions_available()` always `false`; `is_clean()` always `false`**, because `RecoveryGaps::structural()` sets both flags unconditionally | `recovery.rs:160`, `:167–169` | Every replacement briefing reports itself as degraded. Self-documented, but it means the recovery feature is not yet *useful* |
| **P1-5** | **`observe::test_results` is a stub** — it ignores its snapshot argument and returns `None` | `observe.rs:295` | Observed test results never enter the state machine |
| **P1-6** | **`Probe::{TestSuite, Diff, File}` are modelled but always answered `Unverifiable`** | `engine.rs:265`; `PHASE10` limitations | Verification is narrower than the domain model advertises |
| **P1-7** | **Dangling Phase 8 promise** — one adapter instance speaks as one agent; the multi-identity pool was promised in Phase 2 and never built | `PHASE2-ADAPTERS.md:120`; `handoff/adapter.rs:409` | The multi-agent claim in the README is not yet backed by the substrate layer |
| **P1-8** | **Dangling Phase 9 promise** — `RecentContext` is in-memory, unpersisted, and never called | `domain/src/context.rs`; `PHASE12` limitations | The bounded-context engine exists on paper only |
| **P1-9** | **MCP failure cannot affect orchestration state** — the loop never touches the MCP adapters; failure paths are tested only at the adapter layer | `PHASE11` limitations | A dead substrate degrades silently instead of halting or alerting |
| **P1-10** | **~55 `unwrap()`/`expect()` in production `src/`** outside `#[cfg(test)]` | measured | Panics reachable in normal operation; each is a crash of the (future) daemon |
| **P1-11** | **`#![allow(dead_code)]` on the whole `director-store` crate** | `store/src/lib.rs:59` (comment enumerates the offenders) | Hides genuinely unreached code — the same class of defect the `wiring.rs` test was written to catch |

### 7.3 P2 — hygiene

~36 untracked scratch `.txt` logs at the repo root; `create_workbook.py` and `agent_fee.txt` untracked; `tools/sync_readme_example.py` is undocumented and unmentioned in the README; Phase 4's history documented incorrectly in three files (§6.4.2); README badge links to a non-existent `tests/`; contradictory Rust-version prerequisites; unused `uuid`/`sha2` workspace deps; `PHASE12` doc contains a garbled duplicated paragraph; zero tags/releases; single commit author.

### 7.4 Security boundaries — current state

| Boundary | Status | Note |
|---|---|---|
| Memory safety | ✅ **Strong** | 0 `unsafe` blocks; `forbid(unsafe_code)` on 3 of 4 crates (`director-store` does not declare it — add it) |
| Secrets | ✅ **Clean** | 0 matches for `api_key`/`secret`/`password`/token handling in `src/` |
| Substrate coupling | ✅ **Enforced by test** | `tests/boundary.rs` fails the build on any cross-crate substrate reference |
| Path traversal | ⚠️ **Defended but under-tested** | `Repository::validate_path` rejects `..` and rooted-elsewhere paths; **`observer.rs` has no tests of its own**, so this security-relevant code is only exercised transitively (`PHASE2-OBSERVATION.md` says so explicitly) |
| Command execution | ⚠️ **Open by design** | `LocalExecutor` runs arbitrary commands derived from task criteria via `std::process::Command`. Correct for a verification engine; **needs an explicit allowlist/sandbox policy before any untrusted input can reach a criterion** |
| Blocking in async context | ⚠️ **Self-documented** | `LocalExecutor` polls synchronously and is not wrapped in `spawn_blocking` (`executor.rs:10–14`). Fine at current scale; a hazard at daemon scale |
| Subprocess stderr | ✅ | Nulled (`stdio_mcp.rs:169`) — no substrate log leakage |
| Log/Display leakage | ✅ | Transport `Debug` exposes only `next_id` |
| Dependency supply chain | ❌ **Unmanaged** | 103 transitive crates; no `cargo audit`; no `cargo deny` / licence policy file; no Dependabot; no `SECURITY.md`. `rusqlite` uses `bundled` SQLite (good — no system SQLite requirement, but it compiles C) |
| Multi-tenancy / authz | ❌ **Absent** | No authentication, no authorisation, no per-project isolation. Correct to omit pre-launch, but it *is* a production blocker |

---

## 8. Open-Source Readiness Assessment

Three distinct questions, three distinct answers:

| Question | Answer |
|---|---|
| **Is it publicly available on GitHub?** | **Yes.** `github.com/abyyxhek/orqynn`, `main` in sync with `origin`, MIT `LICENSE` present |
| **Is it ready for external contributions?** | **Not yet.** No CI, no CONTRIBUTING, no CODE_OF_CONDUCT, no SECURITY policy, no issue/PR templates, no runnable artifact for a contributor to exercise, plus a live divergent-worktree hazard |
| **Is it ready for production use?** | **No.** No executable, no CI, no release, no versioning, no observability, no security policy |

| Category | Rating | Evidence / gap |
|---|---|---|
| Repository & README quality | **Ready** | Strong README: description, 6 failure modes, 7 core invariants, mermaid control loop, architecture diagram, crate hierarchy, working API example. Minor: badge path broken (§6.4.1), no "Contributing"/"Status" pointers |
| **License & ownership clarity** | **Needs Work** | `LICENSE` exists: **MIT, "Copyright (c) 2026 Orqyn contributors"**. `Cargo.toml` declares `license = "MIT"`. `THIRD_PARTY_LICENSES.md` reproduces both upstream MIT notices. **Gap: a generic "Orqyn contributors" copyright line is not a legal entity.** Before publication, replace with the actual copyright holder(s) and confirm the handoff-mcp / ai-memory MIT attributions are sufficient. **This audit does not assert legal ownership and did not modify the LICENSE.** |
| Installation & quick-start | **Needs Work** | Clone + `cargo build` + `cargo test` are correct and verified. **But the quickstart ends at "run the tests" because there is nothing else to run.** No `cargo install`, no binary, no substrate setup instructions |
| Working examples / reproducible demos | **Missing** | `examples/` is empty. The README code block is excellent but lives inside a markdown file; it is pinned by `tests/readme_example.rs` (executed, passing), which is the correct mitigation — but a first-time contributor still has nothing to *run* |
| API documentation | **Good** | `#![warn(missing_docs)]` on `director-domain` and `director-adapters`; extensive module-level rationale; `cargo doc` will be informative. **Gaps:** no published docs site/ReadTheDocs; no `## Examples` sections on most public functions |
| Architecture documentation | **Excellent** | 13 phase docs, 3,585 lines, with acceptance criteria and *self-declared* limitations — better than most production Rust projects |
| **Automated CI** | **Missing (P0)** | `.github/` does not exist. No build, test, fmt, clippy, audit, or MSRV gate on any push or PR |
| Supported Rust versions & platforms | **Needs Work** | `rust-version = "1.85"`; `rust-toolchain.toml` pins a **Windows-GNU** triple that will not resolve on Linux/macOS. **Only ever validated on Windows** — **NOT VERIFIED** on any other platform |
| Unit & integration testing | **Ready** | 619 tests, 608 passing, 0 failing, 13 ignored. Boundary + wiring regression tests are a genuine strength. **Gaps:** no coverage measurement, no property/fuzz tests, 13 live tests never run in CI |
| External contributor onboarding | **Missing (P0)** | No `CONTRIBUTING.md`, no architecture-onboarding path, no "good first issue", and nothing to run |
| Issue & PR templates | **Missing** | No `.github/` at all |
| Code of Conduct | **Missing** | Not required for MIT, but expected by most maintainers before inviting contributions |
| Security vulnerability reporting | **Missing (P0)** | No `SECURITY.md`, no disclosure channel. `crates/director-app` executes arbitrary commands — this needs a stated policy |
| Dependency health & licence compatibility | **Needs Work** | 103 transitive crates, `Cargo.lock` committed, `THIRD_PARTY_LICENSES.md` present, upstream substrates are MIT. **Gaps:** no `cargo deny`, no automated audit, no update bot, no explicit licence-compat statement for the tree |
| Secrets & unsafe execution paths | **Needs Work** | No secrets, no `unsafe`. But `LocalExecutor`'s command surface has **no documented policy**, and `LocalExecutor` blocks the async runtime |
| Versioning & changelog | **Missing** | All crates at `0.1.0`. **Zero git tags, zero releases.** No `CHANGELOG.md`, no release process |
| Backward compatibility | **Not applicable yet** | Pre-1.0, unpublished, single author. Not a defect — but it should be *stated* |
| Known limitations & roadmap | **Ready** | Unusually honest: each phase doc closes with "What this deliberately does not do", and the README carries a phase-status table. **Gap:** the roadmap stops at Phase 13 |

---

## 9. Complete Future Roadmap

### 9.1 Reconciliation with previously reported milestones

Every previously reported milestone was checked against the tree. **None of Phases 14–22 exists** — there is no code, no doc, and no commit for any of them. They are proposals, not history.

| Phase | Reported as | **Verified reality** |
|---:|---|---|
| 13 — Deterministic Conflict Detection | Planned | ✅ **Confirmed Planned and not started.** `Task.scope_paths` and `Task::overlaps_scope` exist as unused ingredients (`domain/task.rs:205, 296`); the loop never calls them |
| 14 — End-to-End Integration Harness | Planned | ❗ **Materially overlaps existing work.** `tests/end_to_end.rs` (2 tests) and `tests/runtime_recovery.rs` (5 tests) already exist and pass. Phase 11 *is* the integration harness, minus the process boundary. **Reframe, do not duplicate.** |
| 15 — Task Dependency Graph & Advanced Scheduling | Planned | ⚠️ **Partly a duplicate.** The DAG already exists and is validated (`graph.rs`, Kahn; `plan.rs`; `task_dependencies` table). What is genuinely missing is **decomposition** (PLAN deciding *what* the tasks are) and richer scheduling (priority, cost, affinity). **Rescope.** |
| 16 — Policy and Decision Engine | Planned | ❗ **Partly redundant.** `decisions` (table + repository) and `Remediation::{Retry, Rework, Cancel}` already exist. What is missing is a *policy* layer that decides remediation without a caller supplying it. **Rescope to policy-based remediation.** |
| 17 — Multi-Provider & Agent-Runtime Integration | Planned | ⚠️ **Partly existing, partly the dangling Phase 8.** `Harness` enum, two live MCP adapters, `InMemoryProvider` exist. Missing: multi-identity pooling (Phase 8), and real agent-runtime launch. **Merge with the Phase 8 debt.** |
| 18 — Daemon & Orqynn MCP Server | Planned | ✅ **Confirmed Planned.** This is the true P0 blocker (§5). Orqynn is an MCP *client* only; it serves no tools |
| 19 — CLI & Developer Workflow | Planned | ✅ **Confirmed Planned.** Depends on 18 |
| 20 — Observability, Metrics & Tracing | Planned | ✅ **Confirmed Planned.** `tracing` is a declared dep with near-zero use |
| 21 — API / Dashboard | Planned | ❗ **Not justified yet.** Justification requires 18 + 19 to be real and a second user. **Defer to P3** |
| 22 — Security, Reliability, CI, Deployment Hardening | Planned | ✅ **Confirmed Planned — and the CI half is genuinely P0, today** (§8) |

### 9.2 The ordered roadmap

Ordered by dependency, not by the original numbering. **Minimum set for a credible public release = R1, R2, R3, R4, R5, R6.** Everything else is optional or deferred.

---

#### **R0 — Repo hygiene & de-duplication** · **P0** · **Small** · *Do first; it costs under an hour and prevents data loss*

**Goal.** Stop the bleeding before adding features. The divergent worktree (§2.3) and the 42 untracked files (§2.4) are live risks today.
**Tasks.** (1) Decide `../director-brain`'s fate — confirm nothing unique survives, then retire it; **do not merge it forward** (§2.3 explains the silent regression). (2) Delete or archive the ~36 scratch `.txt` logs and `create_workbook.py`/`agent_fee.txt`. (3) Delete `crates/director-app/src/schedule.rs.bak` and `.original` **or** extend `.gitignore` to `*.rs.bak`, `*.rs.original`, `/ORQYNN_PROJECT_AUDIT*.xlsx`, `/Director_Brain_*.xlsx`. (4) Remove the unused `uuid` / `sha2` workspace deps. (5) Fix the README badge path (`tests/` → a real path) and the Rust-version contradiction. (6) Correct the three documents that state Phase 4 never existed.
**Depends on.** Nothing.
**Acceptance criteria.** `git status --porcelain` is empty apart from the two audit reports; exactly one working copy of the remote exists; `git grep "No Phase 4"` returns nothing.
**Validation.** `git status`, `git grep`, `cargo build`.
**Risks.** None, provided `director-brain` is not merged.

---

#### **R1 — CI, security policy & contributor onboarding** · **P0** · **Medium** · *The single highest-value remaining milestone*

**Goal.** Make the existing quality *durable and externally checkable*. Everything Orqynn has achieved so far is protected by one developer running commands by hand.
**Why first.** CI is cheap, entirely orthogonal to the roadmap, and it is the difference between "a good private repo" and "invite contributions".
**Tasks.** (1) `.github/workflows/ci.yml`: `fmt --check`, `clippy -D warnings`, `test --workspace` on **Linux + macOS + Windows**, plus `cargo test --doc`. (2) Add a Linux/macOS toolchain path — remove or parameterise the Windows-GNU pin in `rust-toolchain.toml`. (3) `SECURITY.md` with a disclosure channel and an explicit note that `director-app` executes task-defined commands. (4) `CONTRIBUTING.md`: workspace layout, the four gates, how to run the live suites, the phase-doc convention, the "never mark a file complete without compiling it" lesson. (5) `CODE_OF_CONDUCT.md` (Contributor Covenant). (6) Issue template + PR template. (7) `cargo deny` licence/advisory policy; `cargo audit` in CI; Dependabot. (8) `CHANGELOG.md` + Keep-a-Changelog. (9) Tag `v0.1.0`.
**Depends on.** Nothing (parallel with R0).
**Acceptance criteria.** A clean clone + a single green CI run on all three OSes; a first-time contributor can go README → `cargo test` → green PR with no private knowledge; `SECURITY.md` and `CONTRIBUTING.md` exist.
**Validation.** CI on all three OSes; `cargo deny check`; `cargo audit`.
**Risks.** macOS/Linux failures are **likely on first run** — the project has only ever been validated on Windows (NOT VERIFIED elsewhere). Budget for a portability pass, especially the `git2`/`rusqlite bundled` build.

---

#### **R2 — End-to-End Integration Harness (process boundary)** · **P0** · **Large** · *Reframed from reported Phase 14*

**Goal.** Replace simulated failure with *real* failure. Phase 11 proved crash survival in-process; it explicitly admits *"No process boundary, so no true kill test."*
**Why.** Every durability claim Orqynn makes is currently a claim about test doubles. A single `SIGKILL` mid-observation, mid-assign and mid-verify is the difference between "durable by design" and "durable by assertion".
**Tasks.** (1) A minimal headless harness binary that calls the steps in sequence (this becomes the seed of R3's daemon). (2) Spawn it as a child process; `SIGKILL` it at N injected points: mid-`create_active_plan`, mid-`assign_task`, mid-`checkpoint_round`, mid-`apply_verification`. (3) After each kill, reopen the SQLite file and assert: no partial transaction, no duplicate `AssignmentId`, no re-minted `CheckpointId`, no task in `Done` without a `Verification` row. (4) Restart the harness and assert it resumes from the checkpoint lineage without redoing verified work. (5) Promote the Phase 11 failure matrix (A–L) from prose into executed assertions.
**Depends on.** R0 (hygiene).
**Acceptance criteria.** ≥5 kill-point tests, all passing; every Phase 11 matrix row A–L is an executable assertion; the README's durability claims point at the test that proves them.
**Validation.** The tests themselves.
**Risks.** `SIGKILL` semantics differ on Windows (needs `TerminateProcess` handling) — cross-platform kill injection is the hardest part here.

---

#### **R3 — Phase 13: Deterministic Conflict Detection** · **P0** · **Medium**

**Goal.** Make file-level collisions *detected* rather than hoped away — the project's own headline gap, already scoped in README and `domain/task.rs:204`.
**Why now.** It is the last unimplemented piece of the core invariant set, and it uses machinery that already exists.
**Tasks.** (1) Call `Task::overlaps_scope` from `schedule` (as a soft ranking penalty) and from `assign` (as a hard refusal). (2) Introduce a `ConflictError::ScopeOverlap { task, agent, paths }` and surface it in `ScheduleReport.refused` / `AssignError`. (3) Path normalisation: prefix, separator, trailing-slash, case sensitivity, symlink escape — `overlaps_scope` today is a naive string-prefix test. (4) Allow explicit scope *exemption* for legitimately overlapping tasks. (5) Persist and report the overlap set for in-flight tasks in `MONITOR` so a collision that appears mid-flight is surfaced. (6) A `blockers` table + migration `0004`, closing the dangling `Blocker` debt (P1-2), and raise a `Blocker` on a refused scope overlap.
**Depends on.** Nothing (parallel).
**Acceptance criteria.** Two tasks with overlapping `scope_paths` can never hold active assignments simultaneously, proven by test; disjoint scopes always schedule; the refusal reason is structured and testable; `overlaps_scope` has no remaining dead call sites.
**Validation.** New `crates/director-app/tests/conflict.rs`; property tests over random path pairs would be a genuine plus here.
**Risks.** Path normalisation is deceptively hard (symlinks, `..`, Windows case-insensitivity, WSL paths). Recommend a `path-clean`/`globset` dependency and a fuzz test.

---

#### **R4 — Phase 18: Daemon & Orqynn MCP Server** · **P0** · **Very Large** · *The productisation gate*

**Goal.** Turn the library into a running system. This is the milestone that makes Orqynn a *product* rather than a well-documented set of composable functions.
**Tasks.** (1) New crate `director-loop`: a `#[tokio::main]` driver that runs `OBSERVE → PLAN → SCHEDULE → ASSIGN → MONITOR → VERIFY → REPLAN` on a configurable interval, with graceful shutdown. (2) Config file (`orqynn.toml`) or config trait; per-project project registration; concurrency limits. (3) New crate `director-mcp`: expose Orqynn's *own* tools over MCP stdio (`observe`, `plan`, `schedule`, `assign`, `monitor`, `verify`, `replan`, `checkpoint_status`, `recovery_brief`) so any MCP-capable agent can drive the loop and be driven by it. (4) Wire the substrate adapters into the daemon so Phase 11's "MCP failure cannot affect orchestration state" gap closes — a substrate outage must halt and alert, not degrade silently. (5) A real process boundary: SIGTERM drains, SIGKILL recovers. (6) Structured logging + a run id propagated through every step.
**Depends on.** R2 (the harness becomes the daemon's skeleton) and ideally R1.
**Acceptance criteria.** `orqynd start --project <path>` runs unattended for 24 h with no leaked tasks; the MCP server responds to `tools/list`; an agent can be assigned work and brief itself through Orqynn's MCP tools; a substrate outage is visible in the log and in a metric, not silent.
**Validation.** Integration tests against a real daemon subprocess; the Phase 11 matrix re-run through the daemon.
**Risks.** **Very Large.** This is the project's first true product surface and will surface design gaps that 608 unit tests have hidden. The `Blocker`/`RecentContext`/`ProviderSync` gaps (P1-2, P1-3, P1-8) will all become visible here and should be closed first or in parallel.

---

#### **R5 — Phase 19: CLI & Developer Workflow** · **P1** · **Medium** · *The adoption gate*

**Goal.** Make Orqynn operable by a human without writing Rust.
**Tasks.** (1) `clap`-based CLI: `orqyn init`, `project add`, `daemon start/stop/status`, `plan show`, `task list/show/log`, `assign`, `verify`, `brief <task>` (print the `RecoveryBuilder` package), `checkpoint list`, `db migrate`, `doctor`. (2) Human-readable output (`tabled`/`comfy-table`) plus `--json` everywhere for scripting. (3) `orqyn brief` is the flagship command: it is the visible payoff of Phase 12's work. (4) `cargo install --path crates/orqyn-cli` documented in the README quickstart.
**Depends on.** R4.
**Acceptance criteria.** A new user can install one binary and go from empty directory to a scheduled, verified, recoverable task run without writing code; quickstart in the README is copy-paste and ends in a real command's output.
**Validation.** Shell-level smoke tests; golden-file output tests.
**Risks.** Low. Mostly mechanical once R4 exists.

---

#### **R6 — Release Engineering & Public Release (v0.1.0)** · **P0** · **Small** · *Run in parallel with R1/R4*

**Goal.** Turn a repository into a *release*.
**Tasks.** (1) Resolve the LICENSE copyright holder (see §8 — **the audit does not assert ownership; this is the author's legal decision**). (2) Add `#![forbid(unsafe_code)]` to `director-store` for consistency. (3) `cargo publish --dry-run` for all four (or five, post-R4) crates; verify the dependency ordering publishes correctly. (4) GitHub Release + signed tag + `CHANGELOG.md` entry. (5) crates.io descriptions, keywords, categories, repository/homepage/documentation URLs. (6) A short "Limitations" section in the README stating plainly: research-grade, single-operator, no authn/authz, no multi-tenancy.
**Depends on.** R1; R4 for the daemon crates.
**Acceptance criteria.** `cargo publish --dry-run` clean for every crate; `v0.1.0` released with a changelog; the README's limitations are honest and specific.
**Validation.** `cargo publish --dry-run`; install-from-crates.io smoke test in a clean container.
**Risks.** The LICENSE holder question is a **hard blocker for publication** and is not an engineering decision.

---

#### **R7 — Phase 15 (rescoped): PLAN Decomposition & Advanced Scheduling** · **P1** · **Large**

**Goal.** Close the gap Phase 7's doc names explicitly: *"the planner (decomposition) and the blocker lifecycle are still future work with no number on the roadmap."* Today the caller supplies every `TaskSpec`; PLAN validates and orders, it does not **decide**.
**Tasks.** (1) Decomposition: objective + project state → candidate tasks, under a pluggable `Decomposer` trait. (2) Re-introduce **task priority as a scheduling axis** (Phase 7 deliberately excludes it — that decision should be revisited now that real workloads exist). (3) `depends_on` inference from the graph rather than caller declaration. (4) Critical-path length and estimated cost as scheduling inputs. (5) Close the blocker lifecycle end to end.
**Depends on.** R3 (needs reliable conflict signals to decompose safely), R4 (needs a runtime to learn from).
**Acceptance criteria.** Given an objective and a repository, Orqyn proposes a valid acyclic task set that a human can accept, edit, or reject; priority demonstrably changes assignment order; the blocker lifecycle raises, persists, and resolves.
**Validation.** Property tests over generated DAGs (cycles, self-loops, dangling edges, diamond joins).
**Risks.** Decomposition is the highest-risk item in this roadmap — it is the part that tempts an LLM into the loop and would undo the project's own "judge by machine evidence, not narrative" philosophy. **Recommendation: keep decomposition deterministic and rule-based; do not put a model inside PLAN without a Phase 16 policy layer first.**

---

#### **R8 — Phase 16 (rescoped): Policy & Decision Engine** · **P2** · **Large**

**Goal.** Replace caller-supplied remediations with an explicit, auditable policy.
**Tasks.** (1) A `Policy` trait: given `(task, condition, history, budget) → Remediation`. (2) Ship rule-based policies: retry-once-then-rework, budget-capped, escalate-on-repeated-failure, cancel-if-dependency-dead. (3) Persist policy decisions into the existing `decisions` table with `alternatives_considered` — the schema already supports this. (4) A dry-run mode that prints what a policy *would* do.
**Depends on.** R4, R7.
**Acceptance criteria.** REPLAN can be driven by a policy with no caller-supplied `TaskDecision`; every policy decision is recorded with its alternatives; dry-run changes nothing.
**Validation.** Table-driven tests over synthetic failure histories.
**Risks.** Low technical risk, high design risk — this is where an LLM policy would eventually live, and it must be bounded by the same "machine evidence, not narrative" rule.

---

#### **R9 — Phase 17 (merged with the dangling Phase 8): Multi-Identity Provider Pool & Agent-Runtime Integration** · **P2** · **Large**

**Goal.** Pay off the Phase 2 promise (*"The pool that manages several identities is Phase 8"*) that has sat unfulfilled since 2026-09-25.
**Tasks.** (1) An identity pool: N `AgentId`s over M substrate processes, with session lifecycle and heartbeats per identity. (2) Agent-runtime launch: spawn/attach real harnesses (Claude Code, Codex, …) behind an `AgentRuntime` trait, with a hard timeout and forced-kill path. (3) Close the `extra`-channel round-trip loss: define Orqynn's authoritative fields and reconcile on read (`PHASE2-ADAPTERS.md:120` area). (4) Implement `set_agent_status` at the adapter. (5) Honour `MemoryQuery::tags` instead of ignoring it.
**Depends on.** R4.
**Acceptance criteria.** Orqynn presents ≥3 distinct agent identities to a substrate concurrently; the README's multi-agent claim is backed by an executable test; no round-trip silently drops an Orqynn-authoritative field.
**Validation.** Live MCP suite un-ignored and run in CI against pinned substrate versions.
**Risks.** Substrate API drift. The `PHASE3` doc records a latent regression where a renamed MCP field (`kind` → `type`) shipped undetected for a full phase. **Pin substrate versions and run the live suites in CI — this is the strongest argument for R1's CI work.**

---

#### **R10 — Phase 20: Observability, Metrics & Tracing** · **P1** · **Medium**

**Goal.** Make the loop's behaviour inspectable in production.
**Tasks.** (1) Structured `tracing` spans across all seven steps, with a propagated run/plan/task id. (2) Metrics: tasks by status, lease expiries per hour, verification pass/fail/unverifiable ratio, checkpoint write rate, conflict refusals, step latency. (3) An OpenTelemetry exporter behind a feature flag. (4) A `--verbose` debug mode and a `orqyn doctor` that explains the current state and why nothing is progressing.
**Depends on.** R4.
**Acceptance criteria.** A silent agent, a stuck lease, and a substrate outage are each diagnosable from metrics + logs without reading the database.
**Validation.** A metrics-exporter test; a fault-injection test that asserts the right metric moves.
**Risks.** Low. Should be done *with* R4, not after — retrofitting spans into an async call graph is painful.

---

#### **R11 — Reliability, Security & Deployment Hardening (reported Phase 22, minus CI) · **P2** · **Large**

**Goal.** Production safety, after the product exists.
**Tasks.** (1) **`LocalExecutor` command policy** — an explicit allowlist/program policy, per-project opt-in for arbitrary commands, and an audit log of every executed command. This is the project's largest genuine security surface and currently has no stated policy. (2) Wrap `LocalExecutor` in `spawn_blocking`. (3) Eliminate the ~55 production `unwrap()`/`expect()`; replace with typed errors. (4) Remove `#![allow(dead_code)]` from `director-store`; delete or wire the unreached helpers. (5) Implement `ProviderSync` writing; add event retention to the git observer (currently unbounded). (6) SQLite backup/restore + a documented disaster-recovery runbook. (7) Multi-platform CI matrix hardening; MSRV verification.
**Depends on.** R4, R10.
**Acceptance criteria.** Zero `unwrap()`/`expect()` in production paths; a documented and enforced command policy; a restore-from-backup drill that passes; events bounded.
**Risks.** Medium. The command policy is a genuine behaviour change that will surface in existing task definitions.

---

#### **R12 — API / Dashboard (reported Phase 21)** · **P3** · **Very Large** · *Deferred — not justified*

**Goal.** A read-only HTTP API and web dashboard over loop state.
**Status.** ❗ **Deferred.** Justification requires R4 + R5 to be real and a second user to ask for it. Building a dashboard now would be feature work ahead of product validation.
**Revisit when.** R4 and R5 ship, and someone outside the author asks for it.

---

### 9.3 Roadmap summary table

| ID | Milestone | Priority | Complexity | Depends on | Report number |
|---|---|---|---|---|---|
| R0 | Repo hygiene & worktree de-duplication | **P0** | Small | — | new |
| R1 | CI, security policy & contributor onboarding | **P0** | Medium | — | new (was part of Ph. 22) |
| R2 | End-to-end integration harness (process boundary) | **P0** | Large | R0 | 14 (reframed) |
| R3 | Phase 13 — deterministic conflict detection | **P0** | Medium | — | 13 |
| R4 | Daemon & Orqynn MCP server | **P0** | Very Large | R2, R1 | 18 |
| R5 | CLI & developer workflow | **P1** | Medium | R4 | 19 |
| R6 | Release engineering & public v0.1.0 release | **P0** | Small | R1, R4 | new |
| R7 | PLAN decomposition & advanced scheduling | **P1** | Large | R3, R4 | 15 (rescoped) |
| R8 | Policy & decision engine | **P2** | Large | R4, R7 | 16 (rescoped) |
| R9 | Multi-identity provider pool & agent runtimes | **P2** | Large | R4 | 17 + dangling Ph. 8 |
| R10 | Observability, metrics & tracing | **P1** | Medium | R4 | 20 |
| R11 | Reliability, security & deployment hardening | **P2** | Large | R4, R10 | 22 (minus CI) |
| R12 | API / dashboard | **P3** | Very Large | R5 | 21 (deferred) |

**Total remaining milestones: 13 (R0–R12), of which 6 are P0.**
**Minimum set for a credible public release: R0, R1, R2, R3, R4, R6.**

### 9.4 Recommended immediate actions (next 2 weeks)

1. **R0 today** (≈1 h): retire `../director-brain`, clear the 42 untracked files, extend `.gitignore`. Prevents silent data-model regression.
2. **R1 this week** (≈2–3 days): the CI workflow is the highest value-per-hour item remaining. Expect and budget for first-run Linux/macOS failures.
3. **R3 next** (≈3–5 days): the last core invariant. The machinery exists; the work is calling it and normalising paths.
4. **Resolve the LICENSE copyright holder** (≈1 h, author's call): a hard publication blocker.
5. **Decide the roadmap question the docs keep deferring**: does Orqynn get a driver (R4) or does it stay a library? Everything downstream — CLI, metrics, decomposition — is gated on that single decision.

---

## 10. Reproducible Commands

Environment: Rust toolchain pinned by `rust-toolchain.toml` (`stable-x86_64-pc-windows-gnu`); C toolchain required (`rusqlite` bundles SQLite and compiles C; `git2` needs a C toolchain).

```powershell
# ---- clone and build -------------------------------------------------
git clone https://github.com/abyyxhek/orqynn.git
cd orqynn
cargo build                                  # all four crates
cargo build --release                        # release profile (LTO, strip)

# ---- the four gates (all verified clean on 65cfa02) -----------------
cargo fmt --all --check                      # exit 0
cargo clippy --workspace --all-targets -- -D warnings   # exit 0, zero warnings
cargo test --workspace --all-targets         # 606 passed, 0 failed, 13 ignored
cargo test --workspace --doc                 # 2 passed  -> 608 total

# ---- targeted suites (useful during review) -------------------------
cargo test -p director-domain                # 168
cargo test -p director-adapters              # 77 + 13 ignored live
cargo test -p director-store                 # 29 unit + 69 integration
cargo test -p director-app --test runtime_recovery   # 5   -- Phase 11 proofs
cargo test -p director-app --test end_to_end         # 2
cargo test -p director-app --test recovery_builder   # 18 -- Phase 12
cargo test -p director-app --test checkpoint_lineage # 1  -- the Phase 12 regression

# ---- live MCP suites (NOT run in this audit) ------------------------
# Requires handoff-mcp and ai-memory server binaries on PATH.
cargo test --workspace -- --ignored          # 13 tests: 8 ai-memory + 5 handoff

# ---- there is NO run command, because there is no binary -------------
# Verified: 0 [[bin]] targets, 0 src/main.rs, 0 examples/, 0 benches/.

# ---- Windows + OneDrive note (from the README) ----------------------
# If the checkout lives inside OneDrive, redirect the build directory:
$env:CARGO_TARGET_DIR = "$HOME\.orqyn-target"
cargo test --workspace
```

**Minimum toolchain for a first run:** `cargo build` + `cargo test --workspace` is the entire user-facing story today.

---

## 11. Evidence Index

| Claim area | Primary evidence |
|---|---|
| Repository identity | `git remote -v`; `Cargo.toml:15`; `README.md:468`; `LICENSE` |
| Workspace shape | `Cargo.toml:1–39`; `rust-toolchain.toml`; `.gitattributes` |
| Domain model | `crates/director-domain/src/*.rs` (21 modules); `ids.rs:99–214` (16 IDs); `providers.rs:148–303` (7 traits); `store.rs:216–510` (11 traits) |
| Store & schema | `crates/director-store/src/lib.rs:111–363`; `migrations.rs:63–235`; `migrations/000{1,2,3}*.sql`; `connection.rs:169–238` |
| Control loop | `crates/director-app/src/{observe,plan,schedule,assign,monitor,verify,replan,checkpoint,recovery}.rs`; `engine.rs:115–509`; `lib.rs:39–44` ("the tests are the caller") |
| Scheduling policy | `crates/director-app/src/schedule.rs:141–270`; `docs/PHASE7-SCHEDULE.md` |
| Verification guarantee | `domain/task.rs:149` (`reachable_by_agent_report` excludes `Done`); `domain/verification.rs:233`; `engine.rs:336–509`; `store/lib.rs:363`; `handoff/mapping.rs:81` (`trusted_done_ids`); `docs/PHASE11-RUNTIME-RECOVERY.md` (direct-completion audit) |
| Checkpoint lineage | `crates/director-app/src/checkpoint.rs:211–620`; `store/src/checkpoints.rs`; `domain/checkpoint.rs:105–236`; `tests/checkpoint_lineage.rs` |
| Phase history | `git log --oneline` (43 commits); `docs/PHASE*.md` (13 files, 3,585 lines); `git log -S "Phase 4\|8\|9" -- crates` |
| Test results | `cargo test --workspace --all-targets` and `--doc`, executed 2026-10-16 on `65cfa02` |
| Phase-numbering gaps | `docs/PHASE5-STORE.md` (claims no Phase 4) **vs** `f80184e` + `domain/store.rs:17,350` + `app/checkpoint.rs:1,5` (proves Phase 4 existed); `docs/PHASE2-ADAPTERS.md:120` + 6 source comments (Phase 8); `docs/PHASE0-FORENSICS.md:660` + `domain/context.rs:1` (Phase 9) |
| Open-source readiness | `Test-Path .github` → False; absence of `CONTRIBUTING.md`/`SECURITY.md`/`CODE_OF_CONDUCT.md`/`CHANGELOG.md`; `git tag` → 0; `LICENSE` (MIT) |
| Worktree divergence | `git -C ../director-brain status`; `git diff --stat` (6 files, +318/−20); untracked `director-app/src/checkpoint.rs` (636 lines) |

---

## 12. Statement of Limitations

This audit is **evidence-based and bounded** by the following:

1. **All 13 live MCP tests were skipped.** `handoff-mcp` and `ai-memory` server binaries are absent from this environment. Every claim about live substrate behaviour (the `extra`-channel loss, `MemoryQuery::tags` being ignored, wire-shape bugs) is therefore a **documentary claim**, marked accordingly.
2. **Windows-only validation.** No Linux or macOS build was attempted. Portability is **NOT VERIFIED**.
3. **No code coverage measured.** Test counts are exact; coverage percentages are not claimed.
4. **The two `.xlsx` / `.md` artefacts already present at the repo root** (`ORQYNN_PROJECT_AUDIT.*`, `Director_Brain_Current_Status_and_Roadmap.xlsx`) are **untracked third-party files**, not part of the audited history. They were read only for cross-reference and were **not** modified or overwritten.
5. **The previously reported "end-to-end failures" could not be located** in any repository artefact. They are **NOT VERIFIED** as either real or resolved; no such failure reproduced in this audit.
6. **The roadmap's complexity estimates are the auditor's judgement**, informed by the codebase, not measurements.
7. **No legal assessment was made or implied.** The MIT `LICENSE` exists but carries a generic "Orqyn contributors" copyright line; §8 flags this for the author's decision. **This audit did not create a licence and does not assert ownership.**
8. **No file was modified, committed or pushed** during this audit, other than the two new report files described in §13.

---

## 13. Deliverables

Both files were created with **non-conflicting names**, because `ORQYNN_PROJECT_AUDIT.md` and `ORQYNN_PROJECT_AUDIT.xlsx` already existed at the repo root (untracked, from a prior audit). **`Director_Brain_Current_Status_and_Roadmap.xlsx` was not touched.**

| File | Path | Status |
|---|---|---|
| Markdown report | `C:\Users\ASUS\OneDrive\Desktop\ai_memory\orqyn\ORQYNN_PROJECT_AUDIT_20261016.md` | **New** |
| Excel workbook | `C:\Users\ASUS\OneDrive\Desktop\ai_memory\orqyn\ORQYNN_PROJECT_AUDIT_20261016.xlsx` | **New** (9 worksheets) |

Workbook worksheets: Executive Summary · Phase-by-Phase History · Architecture and Modules · Implemented Features · Test and Validation Results · Bugs, Risks, and Technical Debt · Open-Source Readiness Checklist · Future Roadmap · Immediate Action Plan. All sheets have frozen headers, autofilters, wrapped text, and explicit status/priority columns.
