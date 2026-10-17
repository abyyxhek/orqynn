# Orqynn — Project Audit, Development History & Open-Source Readiness Report

**Audit date:** 2026-10-16
**Auditor:** opencode (automated repository audit)
**Repository:** `https://github.com/abyyxhek/orqynn.git` (remote `origin`)
**Local path audited:** `C:\Users\ASUS\OneDrive\Desktop\ai_memory\orqyn`
**Commit audited:** `65cfa02` — *"docs: sync README with the real API; pin the example with a compiled test"* (2026-10-14)
**Method:** all claims below were verified by reading source, Git history, and documentation, and by executing build/test/lint gates on the working tree. Items that could not be verified are explicitly labelled **Not Verified**.

> **Naming note.** This report covers **Orqynn** only. It is unrelated to the similarly-named "Orqyn" (the pre-rebrand working name that survives inside this tree as `crates/director-*` and the `aimemory`/`handoff` adapter module names) and entirely unrelated to "SNAGLINE". The pre-rebrand name is a historical artifact, not a separate project.

---

## 1. Executive Summary

Orqynn is a **well-engineered, well-tested orchestration library that has no runnable entry point**. The engineering quality of the internal architecture is genuinely high — better than most production Rust projects — and the correctness reasoning is documented to an unusual standard. What is missing is the entire outer shell that turns a library into a product.

**Verified state at a glance:**

| Metric | Value | How verified |
|---|---:|---|
| Git commits | 43 | `git log --oneline \| wc` |
| Date span | 2026-09-25 → 2026-10-14 | `git log --format=%ad` |
| Branch / ahead-behind | `main`, 0 ahead / 0 behind | `git rev-list --left-right --count` |
| Working tree | Clean (tracked); 44 untracked scratch files | `git status` |
| Crates | 4 | `Cargo.toml` workspace members |
| Rust source files | 50 (`src/`), 17 (`tests/`) | filesystem scan |
| Source lines | 21,032 (`src/`) + 10,868 (`tests/`) | line count |
| SQL migrations | 3 (258 + 57 + 36 lines) | `crates/director-store/migrations/` |
| Documentation | 13 markdown files, 3,585 lines | `docs/` |
| **Tests passing** | **608** | `cargo test --workspace` |
| **Tests failing** | **0** | same |
| Tests ignored (live MCP) | 13 | same |
| Test suites | 25 | same |
| `cargo fmt --check` | Clean | executed |
| `cargo clippy --all-targets` | Clean, 0 warnings | executed |
| Direct dependencies | 12 (2 notable: `git2`, `rusqlite`) | `Cargo.lock` (103 total crates) |
| Executables / bins | **0** | no `[[bin]]`, no `examples/`, no `benches/` |
| CI workflows | **0** (`.github/` does not exist) | `Test-Path .github` → False |

**Bottom line for a senior reviewer:** this is a **library-quality alpha**. The orchestration core — the part that is genuinely hard and genuinely novel — is built, tested and internally consistent. There is no CLI, no daemon, no MCP server, no binary, no CI, no contribution guide, and no security policy. An external developer cannot run Orqynn; they can only read it and compile its tests.

---

## 2. Repository Verification

### 2.1 Identity and Git state

| Property | Value |
|---|---|
| Remote (fetch) | `https://github.com/abyyxhek/orqynn.git` |
| Remote (push) | `https://github.com/abyyxhek/orqynn.git` |
| Branch | `main` |
| HEAD | `65cfa02` |
| Ahead of origin | 0 |
| Behind origin | 0 |
| Tracked modifications | None |
| Untracked files | 44 (see §2.4) |
| Tags | None |
| Authors | 1 (`Abishek`) |

This **is** `abyyxhek/orqynn`. Confirmed against `git remote -v`, not against documentation claims.

> **Important discovery — two local clones exist.** A second checkout exists at `C:\Users\ASUS\OneDrive\Desktop\ai_memory\director-brain`, pointing at the *same* remote but parked at `37f39b8`, which is **one commit behind** the audited clone. Its HEAD message (`docs: correct README API and compile example as doctest`) is a divergent sibling of `65cfa02`. This is exactly the hazard that `docs/AUDIT-2026-10-04.md` flagged as finding **F4** ("a stale duplicate checkout … a reader who opens the wrong directory sees a project that is four phases behind"). It is still live, though now only one commit stale rather than four phases. **Recommend deleting or clearly marking the `director-brain` clone.**

### 2.2 Workspace structure

```
orqyn/
├── Cargo.toml              # workspace, 4 members, v0.1.0, edition 2021, MSRV 1.85
├── Cargo.lock              # 103 crates
├── LICENSE                 # MIT, "Copyright (c) 2026 Orqyn contributors"
├── README.md               # 468 lines
├── THIRD_PARTY_LICENSES.md # upstream notices (handoff-mcp, ai-memory)
├── rust-toolchain.toml     # channel = stable-x86_64-pc-windows-gnu, profile minimal
├── crates/
│   ├── director-domain/    # 18 src files, ~6.9k lines — pure model, zero substrate coupling
│   ├── director-adapters/  # 13 src + 4 test files — git2, MCP stdio clients, LocalExecutor
│   ├── director-store/     # 13 src + 1 test (3,272 lines) — SQLite repositories
│   └── director-app/       # 10 src + 12 test files — the control loop
├── docs/                   # 13 phase/audit documents + banner.png
├── migrations/             # (under director-store) 0001, 0002, 0003
└── tools/sync_readme_example.py
```

### 2.3 Database schema

Three ordered, immutable SQL migrations, embedded at compile time via `include_str!` (`crates/director-store/src/migrations.rs:63-90`):

| Version | Name | File | Lines |
|---:|---|---|---:|
| 1 | initial | `0001_initial.sql` | 258 |
| 2 | plans_decisions | `0002_plans_decisions.sql` | 57 |
| 3 | verifications | `0003_verifications.sql` | 36 |

Tables created by migration 1 (verified by the test at `migrations.rs:269`): `projects`, `repositories`, `tasks`, `task_dependencies`, `task_status_history`, `agents`, `agent_sessions`, `agent_assignments`, `checkpoints`, `project_states`, `provider_sync`; plus `plans`, `decisions` (migration 2) and verification tables (migration 3).

**Migration design is a genuine strength.** The rules are enforced and tested:
- Ordered, each in its own transaction; a failed migration leaves **no** tracking row (`migrations.rs:425`).
- Contiguity from 1 is asserted (`migrations.rs:222`) so a gap cannot silently stop an upgrade.
- Re-running is a no-op (`migrations.rs:259`), which is what makes `Store::open` safe on every start.
- A database **newer** than the build is **refused**, never silently downgraded (`migrations.rs:449`).

### 2.4 Untracked scratch files (44)

All 44 are untracked and left in place per the preservation constraint. They are a hygiene problem: 40+ `.txt` files containing stale cargo output from intermediate development states.

**Specifically relevant to this audit:** `test_e2e.txt` and `full_test_output.txt` record **historical E2E failures** (`4 passed; 4 failed` and `2 passed; 6 failed`) naming tests `replacement_agent_recovery_context`, `restart_persistence`, and `replanning_unresolvable_dependency`. These were investigated (§6.3) and are **stale, not current**. None of those three test functions exists anywhere in the tree today. This is important: those logs are the likely source of the "previously reported end-to-end failures", and a reader who opened them without checking the current tree would reach a wrong conclusion.

Also untracked: `crates/director-app/src/schedule.rs.bak` and `crates/director-app/src/schedule.rs.original` (leftover backups inside `src/`), `create_workbook.py`, and `Director_Brain_Current_Status_and_Roadmap.xlsx` (a prior deliverable, explicitly preserved).

---

## 3. Development History, Phase by Phase

Phase numbering below is **the repository's own**, reconstructed from commit messages, `docs/` documents, and source. Gaps (4, 8, 9) are explained, not invented.

### Phase 0 — Repository Forensics
- **Document:** `docs/PHASE0-FORENSICS.md` (724 lines, 2026-09-24)
- **Objective:** read-only forensic audit of two upstream substrates — `akitaonrails/ai-memory` 2.4.0 and `alphaelements/handoff-mcp` 0.35.1 — to derive Orqyn's integration boundary.
- **Delivered:** the governing rule of the whole project — *"Orqyn talks to both substrates exclusively over MCP, never by importing their internal storage structs."* Plus risks R1–R8, and a reuse/wrap/fork matrix concluding **USE both via MCP, wrap behind adapters, do not fork**.
- **R6 is the thesis of the project:** *"handoff-mcp's verification is not verification. `handoff_check_criterion` is a self-report."*
- **Status:** Complete.
- **Caveats the document itself declares:** neither upstream repo was built or run (*"no Rust toolchain present in this environment"*); runtime behaviour unverified.

### Phase 1 — Canonical Domain Model and Provider Boundary
- **Document:** `docs/PHASE1-DOMAIN.md`
- **Objective:** domain model and provider traits with zero substrate coupling.
- **Key invariants:** task identity ≠ agent identity (sealed distinct newtypes); **`Done` is not reachable by agent report**; seven async provider traits; *"Nothing here completes a task."*
- **Boundary enforced by test:** `crates/director-adapters/tests/boundary.rs` fails the build if any `.rs` outside adapters contains `handoff_mcp`/`ai_memory`.
- **Status:** Complete (8/8 acceptance boxes checked in the document).
- **Verified still true:** the boundary test is present and passing.

### Phase 2 — Substrate Adapters and the Git Observation Layer *(two halves, same phase number)*
- **Documents:** `docs/PHASE2-ADAPTERS.md`, `docs/PHASE2-OBSERVATION.md`
- **Objective:** make the Phase 1 traits real against something other than a `HashMap`.
- **Delivered:** `HandoffAdapter` (stdio JSON-RPC MCP, four-layer `wire`/`transport`/`mapping`/`adapter` shape); `git2`-based read-only `GitObserver` / `RepositoryStore` / `GitService`.
- **Load-bearing decisions:** a substrate `done` **never** becomes Orqyn `Done` — it becomes `VerificationPending`; the `extra` channel is closed in both directions; identity is process-global (one adapter = one agent); `git2` chosen over the git CLI to shrink the injection surface; untracked → `FileAdded` and conflicted → `FileModified` so an observation is never a silent no-event.
- **Status:** Complete.
- **Defect found and fixed:** two wire-shape bugs (`done_criteria.text` vs `item`; `TaskLink{kind,url}` vs `{target,link_type}`).
- **Honest gaps the document records, still open today:** `observer.rs` has no tests of its own despite security-relevant path validation; `service.rs` claims coverage from a `tests/git_integration.rs` that **does not exist**; no event-retention policy.
- **Promised-then-abandoned:** this document promises *"The pool that manages several identities is Phase 8."* **Phase 8 never happened.** See §3.5.

### Phase 3 — The ai-memory Adapter
- **Document:** `docs/PHASE3-MEMORY.md`
- **Objective:** a second real `MemoryProvider` implementor.
- **Delivered:** `AiMemoryAdapter` over stdio JSON-RPC; four documented model mismatches handled (static-client scoping; `rank` meaning two different things on `memory_query` vs `memory_recent`, handled via `RankMeaning`; bodies fetched not snippeted; titles smuggled as markdown H1 to dodge a JSON-escaping bug).
- **Status:** Complete.
- **Major latent defect found and fixed:** `stdio_mcp.rs` read the `kind` field where the wire used `type` — *"Every tool call — handoff and ai-memory alike — was failing."* It survived all of Phase 2 undetected because the live suite was not re-run. The document's distilled lesson: *"source reading tells you what a struct contains; only a live run tells you what reaches the wire."*

### Phase 4 — **Does not exist. Deliberate.**
This is stated explicitly and independently in three documents: `PHASE5-STORE.md`, `AUDIT-2026-09-30.md`, and `AUDIT-2026-10-04.md` — *"No Phase 4. The roadmap jumps 3 → 5 … Not a gap in the numbering."* There is no Phase 4 code, document, or record of intent. **Do not treat this as missing work.**

### Phase 5 — The Store
- **Document:** `docs/PHASE5-STORE.md` — *"Status: complete."*
- **Objective:** persist everything Orqyn *owns*. Before this, a checkpoint/plan/decision/assignment/verification existed only for the lifetime of the process.
- **Delivered:** `director_store::Store`, a `Store` marker supertrait, eight repository traits, three migrations, optimistic concurrency via `state_version`.
- **Three invariants expressed in storage, not convention:** `tasks` has **no** `current_agent_id` column (ownership is derived through assignments); nothing is deleted (`released`/`superseded` + append-only history); partial unique indexes enforce at most one active assignment and one current checkpoint per task.
- **Status:** Complete.
- **Two significant defects found and fixed — both are strong evidence of engineering rigour:**
  1. **The partial unique indexes were inert.** Enum status is stored as JSON-quoted text (`"active"`, 8 chars) but the index predicate compared bare `active` — *"It matched nothing. The index existed, was documented as the load-bearing invariant, and enforced nothing at all."* Schema-level unit tests missed it because they inserted bare literals; only the integration test caught it.
  2. **`assign_task` left the departing agent still holding the task** after reassignment.
- **Known outstanding (still true today):** five store helpers remain `#![allow(dead_code)]`; `ProviderSync` has a repository and no caller; `Blocker` is not persisted at all.

### Phase 6 — The Control Loop
- **Document:** `docs/PHASE6-APP.md` (572 lines) — *"Status: complete."*
- **Objective:** the first crate that *composes* the earlier layers. Nothing in the workspace had ever opened a `Store`.
- **Delivered as six separately-landed commits, in loop order:**
  - **OBSERVE** — composes `GitService` (raw evidence) with `Store` (normalized belief); round is idempotent.
  - **PLAN** — validates the dependency graph (no self-deps, unknown deps rejected, cycles rejected with the cycle path), topological ordering; plan + tasks + activation in **one transaction**; supersede-not-delete gives at most one active plan as an invariant rather than caller discipline.
  - **ASSIGN** — four legality checks (active plan, task in plan, not already assigned, status `todo`, deps satisfied, agent available, capabilities present) **all before any write**; one transaction via `assign_and_start`.
  - **MONITOR** — `Working`/`Stale`/`Expired`/`Orphaned`; liveness **derived, never stored**; `expire_lease` is one transaction. `report_done` moves to `verification_pending` **and stops there**.
  - **VERIFY** — the only step that may reach `done`; pure `judge()` with documented precedence; *"An unrunnable check is not a failure"* — it is `Unverifiable`.
  - **REPLAN** — three trigger conditions, three remediations (`retry`/`rework`/`cancel`); validate-all-then-apply-all so a round is all-or-nothing; *"The cascade is reported, not written."*
- **Status:** Complete.
- **The load-bearing statement in this phase's own documentation:** *"Not an MCP server, and not a binary yet … nothing yet drives them … **the tests are the caller**."*

### Phase 7 — The Scheduler
- **Document:** `docs/PHASE7-SCHEDULE.md` — *"Status: complete."*
- **Objective:** remove the one judgment that does not need a human — which ready task goes to which available agent.
- **Delivered:** `schedule::schedule` composes `assign` and adds **no new store method and no new domain entity**. Pure `match_agents` with four ordered rules: eligible → fresh-eyes-first (a preference, never a prohibition) → specialist-first (fewest declared capabilities) → agent-id tie-break. Deliberately **not** a priority axis, because *"the plan's order is already the priority order; a second priority system would disagree with the first."*
- **Status:** Complete.
- **Design honesty worth noting:** a proposal the store refuses is returned as `Refused`, **not** raised as an error — *"Proposals are a snapshot; the write is live."* This keeps a world that changed mid-round from masquerading as a transient fault.

### Phase 8 — **Promised, never delivered.**
`PHASE2-ADAPTERS.md` states once: *"The pool that manages several identities is Phase 8."* No document, no code, no later mention. Phase 11 later confirmed the opposite state: *"no step of the loop makes an MCP call."* **This is a genuine dangling promise** — the multi-identity pool is exactly the mechanism that would let Orqyn drive several agents through handoff-mcp, and it is the substantive gap that the driver/MCP-server roadmap thread must close.

### Phase 9 — **Aspirational label only.**
Referenced exactly once, in `PHASE0-FORENSICS.md`: *"Control flow (Phase 9 onwards …)"*. The name never recurs. The loop actually built in Phase 6 differs from the Phase 0 sketch (no `UNDERSTAND` step; `SCHEDULE` was inserted later at Phase 7). **Not real history — do not report it as a phase.**

### Phase 10 — The Verification Engine
- **Document:** `docs/PHASE10-ENGINE.md` — *"Status: complete."*
- **Objective:** *"the record layer existed … What had no caller was the engine."*
- **Delivered:** `director-app::engine::verify_task` — the **only writer of a verdict**. It *"takes no agent id, reads no agent report, and consults no session."* Five-step round: resolve probes → gather evidence → judge → pin state version → land. Four-rule precedence: `Failed` decides first, `Unverifiable` second, `Passed` third, **`Observed` never decides**. A stale round loses the whole transaction as a `StateVersionConflict`.
- **Status:** Complete.
- **Declared incompleteness (still true):** only `Probe::Command` is implemented — `TestSuite`, `Diff`, and `File` are modelled in the domain and answered as `Unverifiable`; the engine does not interpret evidence beyond precedence — *"Judging prose is a later phase's work."*

### Phase 11 — Runtime & Recovery Validation
- **Document:** `docs/PHASE11-RUNTIME-RECOVERY.md` — *"Status: complete."*
- **Objective:** explicitly *adds nothing*; it proves the property the architecture exists for.
- **Delivered:** a 12-row failure matrix (A–L) showing no data loss and no false completion in any row; a workspace-wide audit finding six `UPDATE tasks SET status` sites, of which **exactly one** — `Store::apply_verification` — can write `Done`; five new integration tests.
- **Status:** Complete.
- **Value:** this phase established the project's most important measured claim — the invariant is not just documented, it is *auditable*.
- **Defects found and fixed:** 8 failing adapter tests caused by a test helper keying temp dirs by PID and colliding after PID reuse; and the discovery that `fmt --check` and `clippy` were in fact **failing** on the tree the document described as clean. The project's standing rule emerged here: *"a gate result certifies the exact tree it ran on and nothing said about it since."*
- **Limitations recorded, and their current status:**

| Limitation (as written in Phase 11) | Status today |
|---|---|
| No process boundary, so no true kill test | **Still open** |
| No loop step writes a checkpoint | **Resolved in Phase 12** |
| `Blocker` not persisted at all | **Still open** |
| `ProviderSync` never written | **Still open** |
| OBSERVE depends on concrete `GitService`, not a trait | **Still open** |
| MCP failure tested at the adapter, not in the loop | **Still open** |

### Phase 12 — Automatic Checkpointing and Recovery Context
- **Document:** `docs/PHASE12-CHECKPOINT-RECOVERY.md` — *"Status: complete and committed."* Commits `bda3382` (2026-10-13) and `65cfa02` (2026-10-14).
- **Objective:** close Phase 11's two gaps — a checkpoint writer, and a single place that assembles a replacement agent's continuation context.
- **Delivered:** `checkpoint::checkpoint_round` runs **inside OBSERVE** (not MONITOR) — the reasoning is excellent and worth repeating: *"MONITOR has no git state at all; a checkpoint it wrote would be a narrative with no evidence behind it, which is precisely the self-reporting shape the loop's invariants exist to prevent."* Lineage-derived ids (`CHK-<task>-3`); a `restates_current` guard so no-op rounds write nothing; `recovery::RecoveryBuilder` producing a `Recovery` bundle with a resume verdict of `Ready`/`Stale`/`Conflicted`/`Blocked`/`Insufficient` via `Checkpoint::compare_with` — a guard that had existed since Phase 5 with no caller, and now has one; SCHEDULE attaches it to the assignment.
- **Status:** Complete.
- **Defect found and fixed:** the original code re-minted `CHK-work-1` for a task that already had it, failing the whole observation round on a primary-key violation. Pinned by regression test in `tests/checkpoint_lineage.rs`.
- **The "finished edit":** the document ends on an open design question and then resolves it in place — `CheckpointRound` gained a `conflicts` field so a per-task checkpoint conflict is *recorded and the round continues*, with `retry_against()` carrying the cure. This is a good example of the project documenting its own reasoning rather than hiding it.
- **Explicitly declared gaps:** *"It does not add a process. There is still no daemon, no driver, and no MCP server … `director-app` remains a library whose caller is the tests."* Also: `RecentContext` and `Blocker` are not persisted; no automatic agent resume.
- **Notable discipline:** *"No new tables were added to make the package look complete."* — which is precisely why `RecoveryGaps::is_clean()` is `false` for every build today.

### Summary table

| Phase | Name | Status | Key evidence |
|---:|---|---|---|
| 0 | Repository Forensics | Complete | `PHASE0-FORENSICS.md`; doc-only, upstream never built |
| 1 | Domain Model & Boundary | Complete | `PHASE1-DOMAIN.md`; `tests/boundary.rs` passing |
| 2 | Adapters & Observation (2 halves) | Complete | `PHASE2-ADAPTERS.md`, `PHASE2-OBSERVATION.md` |
| 3 | ai-memory Adapter | Complete | `PHASE3-MEMORY.md` |
| 4 | — | **Does not exist, deliberate** | stated in 3 documents |
| 5 | Durable Store | Complete | `PHASE5-STORE.md`; `tests/store.rs` (3,272 lines) |
| 6 | The Control Loop (6 steps) | Complete | `PHASE6-APP.md`; 6 integration suites |
| 7 | Intelligent Scheduler | Complete | `PHASE7-SCHEDULE.md`; `tests/schedule.rs` |
| 8 | Multi-identity pool | **Promised, never delivered** | one mention in `PHASE2-ADAPTERS.md` |
| 9 | — | **Never adopted** | one aspirational mention in `PHASE0` |
| 10 | Verification Engine | Complete | `PHASE10-ENGINE.md`; `tests/engine.rs`, `tests/end_to_end.rs` |
| 11 | Runtime & Recovery Validation | Complete | `PHASE11-RUNTIME-RECOVERY.md`; `tests/runtime_recovery.rs` |
| 12 | Checkpointing & Recovery Context | Complete | `PHASE12-CHECKPOINT-RECOVERY.md`; `tests/recovery_builder.rs` |

**Latest verified completed phase: Phase 12.** **Current active phase: none** — no Phase 13+ work exists in code or commits.

---

## 4. Architecture

### 4.1 The problem Orqyn solves

Coding agents (Claude Code, Codex, Cursor, Goose, Devin, OpenCode, Aider) are individually excellent at producing local code changes. Substrate servers (handoff-mcp, ai-memory) provide session persistence and knowledge retrieval. **What does not exist in the ecosystem is a supervisory layer** — something that decides *what* the work is, *who* does it, *whether it actually succeeded*, and *what happens when it did not*.

The project's central technical claim is that **an agent's report is not evidence**. `handoff-mcp`'s `handoff_check_criterion` is a self-report; Orqyn's `R6` finding states plainly that this *"is not verification."* Consequently Orqyn runs the checks itself and treats a task's completion as unreachable except through its own verification engine.

### 4.2 Why orchestration is separated from runtimes and substrates

Three independent reasons, all evidenced in the code:

1. **Vendor independence.** The core imports no agent SDK and no substrate crate. `crates/director-adapters/tests/boundary.rs` makes this a *build failure*, not a convention — if a substrate type name appears in any file outside `director-adapters`, the test suite does not compile. This is enforced at CI-less time by a test, which is the strongest available mechanism without CI.
2. **Substrates are competing data models, not tools.** `PHASE2-OBSERVATION.md` draws the line precisely: a *tool* imports a data format; a *substrate* carries a competing data model. Coupling to either would make Orqyn's state subordinate to someone else's schema. Hence `ProviderSync` is *"a pointer and a status… never a replica."*
3. **Testability.** Because the loop makes **zero MCP calls** and touches only its own SQLite file, the entire control loop can be integration-tested against real git repositories, a real database and a real process executor with no network and no servers. This is verified fact, not aspiration — it is exactly why 608 tests run in seconds.

### 4.3 The four crates

| Crate | Responsibility | Depends on |
|---|---|---|
| **director-domain** | Canonical entities, state machines, sealed newtype IDs, dependency-graph algorithms, the 7 provider traits. Encodes invariants in types. | serde, chrono, thiserror, async-trait, sha2. **No substrate, no I/O.** |
| **director-adapters** | The hexagonal edge: `InMemoryProvider` (fakes), `LocalExecutor` (real `std::process`, 60s default timeout), `GitObserver`/`RepositoryStore`/`GitService` (read-only `git2`), `HandoffAdapter` and `AiMemoryAdapter` (stdio JSON-RPC MCP), shared `stdio_mcp` transport. | domain, git2 |
| **director-store** | SQLite persistence: repositories, migrations, optimistic concurrency, partial unique indexes, `apply_verification` transaction. | domain, rusqlite (bundled) |
| **director-app** | The control loop: OBSERVE, PLAN, SCHEDULE, ASSIGN, MONITOR, VERIFY, REPLAN, plus `engine`, `checkpoint`, `recovery`. Declares `unsafe_code = "forbid"`. | domain, adapters, store |

The dependency graph is acyclic and strictly layered. `director-app` is the only crate that composes the others, and it holds no state of its own.

### 4.4 The control loop

```
OBSERVE → PLAN → SCHEDULE → ASSIGN → MONITOR → VERIFY → REPLAN
```

Each step is a small module composing the layers beneath it, landed and tested one at a time in the order it runs. (`director-app/src/lib.rs:13-14` still shows the older six-step form without SCHEDULE — a harmless doc staleness in a crate-level comment.)

- **OBSERVE** — reads git independently of any agent, normalizes into the store, and (since Phase 12) checkpoints in the same round. Idempotent: the same observation is one event, not two.
- **PLAN** — validates and persists a caller's decomposition. It does **not** invent tasks; decomposition still arrives from a caller.
- **SCHEDULE** — decides pairings with a pure four-rule policy, applies them through `assign`.
- **ASSIGN** — enforces legality, one transaction, refuses rather than displacing a sitting holder.
- **MONITOR** — derives liveness, expires dead leases, acknowledges sessions, and receives `report_done`.
- **VERIFY** — surveys tasks awaiting a verdict and delegates judgment to the engine.
- **REPLAN** — answers for work the loop's *own verdicts* stopped. MONITOR is for leases; REPLAN is for verdicts and orphans. That separation is deliberate and documented.

### 4.5 Task lifecycle, scheduling, leases, concurrency

Status flow: `todo → in_progress → verification_pending → done | failed`, with `blocked` and `cancelled` reachable.

- **Scheduling policy (Phase 7):** eligible → fresh-eyes-first → specialist-first → agent-id tie-break. One task per agent per round. Task priority is deliberately not an axis.
- **Leases:** an assignment carries a lease with 30/60-minute freshness windows; `Working`/`Stale`/`Expired`/`Orphaned` are **derived, never stored**. `expire_lease` is a single transaction.
- **Optimistic concurrency:** a `state_version` write counter on every mutable row. A stale writer loses the whole transaction as a `StateVersionConflict`. Phase 11 audited this at the call-site level: six sites can write task status, exactly one can write `Done`.
- **Conflict detection — an important correction.** The prompt's framing of "Phase 13 — Deterministic Conflict Detection" as a *missing* capability is accurate, but the term is easily misread. Orqyn already has **store-level** concurrency conflict handling: optimistic version guards, partial unique indexes (one active assignment, one current checkpoint per task), and `TaskAlreadyAssigned` refusal. What Phase 13 proposes is **file/worktree-level** conflict detection using `scope_paths` — detecting that two agents have been pointed at the same files. Those are different problems and the roadmap should not conflate them.

### 4.6 Persistence, checkpoint lineage, restart behaviour

- **Append-only by policy:** nothing is deleted. Assignments are `released`, checkpoints `superseded`, plans superseded, and history rows accumulate. `Checkpoint::compare_with` computes a resume verdict; lineage ids are derived (`CHK-<task>-3`) rather than drawn from a counter, which is what broke in Phase 12.
- **Restart semantics:** *"restart" means reopening one SQLite file.* Phase 11 proved this at the store level across 11 entities, and `tests/runtime_recovery.rs` covers persistent-state survival, an abandoned process leaving committed state intact, and a vanished agent's lease being reclaimed so a replacement can finish.
- **Continuation context:** `RecoveryBuilder` assembles objective, progress, changed files, decisions, assumptions, next action, and a resume verdict into one `Recovery`, which SCHEDULE attaches to the assignment as `Scheduled { recovery: Some(..) }`.
- **Honest gap:** `RecoveryGaps::is_clean()` is `false` in every build, because `RecentContext` and `Blocker` have no table and no repository. This is surfaced rather than papered over.

### 4.7 Verification guarantees

This is the strongest part of the system and the best-evidenced claim in the repository:

1. **`Done` is unreachable by agent report.** Enforced in the type system (`reachable_by_agent_report`) and at runtime — `report_done` stops at `verification_pending`.
2. **The engine is independent of the agent.** `verify_task` takes no agent id, reads no agent report, consults no session.
3. **The engine is the only writer of a verdict**, and it writes in one transaction (`Store::apply_verification`): the verification row, the task status move, and the history row together.
4. **`Unverifiable` is not `Fail`.** A check that cannot be run does not condemn the work and does not pass it either; it blocks a pass and says why.
5. **`Observed` never decides.** Only the terminal states decide.
6. **Failures are permanent records.** History is append-only, so a retry lands a *second* verdict beside the first rather than erasing it.

### 4.8 Adapters and the external MCP substrate

`HandoffAdapter` and `AiMemoryAdapter` are real, complete stdio JSON-RPC MCP clients in four layers each (`wire` / `transport` / `mapping` / `adapter`), with three live `#[ignore]`-marked integration suites each (13 ignored tests total) that are run against real servers and are documented as passing against handoff-mcp v0.35.1 and ai-memory v2.4.0.

**However — the critical structural fact:** `director-app` has **no reference to either adapter outside its tests**. The adapters are built, correct, and **not wired into the loop**. They are a finished integration surface with no caller. Phase 0's rule (*MCP only, never import internals*) is fully honoured; the connection itself is simply not made yet.

---

## 5. What Is Implemented, Experimental, Simulated, and Planned

### Implemented and genuinely production-grade *as a library*
Domain model and invariants; SQLite persistence with migrations and optimistic concurrency; the six-step loop plus SCHEDULE; the verification engine and its precedence rules; the scheduler policy; leases and liveness derivation; checkpointing inside OBSERVE; recovery-context assembly; append-only audit history; the git observation layer; both MCP adapters.

### Implemented but deliberately partial
- **Verification probes:** only `Probe::Command`. `TestSuite`, `Diff`, `File` are modelled but answered `Unverifiable`.
- **Evidence interpretation:** precedence rules only. No judging of prose.
- **Memory publish:** best-effort with a 10-second timeout; failures are recorded rather than propagated.
- **`OBSERVE`:** coupled to the concrete `GitService` rather than the `ProjectStateProvider` trait.

### Simulated in tests (real code paths, substituted edges)
- **The agent itself.** No test runs a real coding agent. The tests drive Orqyn's own steps and use `LocalExecutor` to run *real shell commands* (`git --version`, `git --no-such-flag`) as verification probes — so verification is genuinely exercised, but the agent's side of the contract is asserted, not simulated end-to-end.
- **Adapters in loop tests:** `InMemoryProvider` substitutes for MCP substrates inside the test suite. The real MCP paths are covered only by the 13 ignored live tests, which were **not run in this audit** (they require live external servers).

### Planned / absent
Driver process; daemon; Orqynn MCP server; CLI; decomposition (PLAN choosing *what* the tasks are); file-scope conflict detection; multi-identity agent pool (abandoned Phase 8); persisted `RecentContext` and `Blocker`; a real SIGKILL test; event retention.

---

## 6. Codebase Assessment

### 6.1 Strengths (evidence-backed)

- **Invariants are in types, not comments.** Sealed newtype IDs make task/agent/session identity confusion a compile error. `#[must_use]`-style discipline and exhaustive `match` on statuses. This is the single best decision in the codebase.
- **Invariants are in the schema.** Partial unique indexes back two load-bearing rules, and — crucially — a unit test was written that *silently passed against a broken index*, then caught by integration test. Finding that and fixing it is exactly the behaviour you want from an engineer.
- **The correctness reasoning is written down.** Every step documents *why* it refuses, not just what it does. Error enums are flat and every variant's doc comment explains which caller reaction it implies. Errors name the offending ids because *"invalid graph" is not actionable*.
- **Self-auditing.** Phase 11 mechanically counted the `UPDATE tasks SET status` sites and proved only one can write `Done`. Very few projects do this unprompted.
- **Honesty about gaps.** Phase documents contain explicit *"Known limitations"*, *"What this deliberately does not do"*, and *"Honest gaps"* sections. The project repeatedly declines to add fake completeness (*"No new tables were added to make the package look complete"*).
- **Test philosophy is integration-first.** Every step's suite runs against a real store and, where relevant, a real git repo and a real subprocess. Stated rule: *"No test constructs the rows it then claims to verify."*
- **`tests/wiring.rs`** fails the build if any `.rs` under `src/` is not reachable from the module tree — a direct structural answer to the Phase 6 incident where 610 lines of `plan.rs` sat uncompiled.

### 6.2 Weaknesses and risks

| # | Finding | Severity | Evidence | Status |
|---|---|---|---|---|
| **F1** | **No runnable artifact.** No binary, CLI, daemon, or MCP server. | **Critical (for adoption)** | No `[[bin]]` in any `Cargo.toml`; no `examples/`; `director-app/src/lib.rs:39-44` states it plainly | Open |
| **F2** | **No CI.** `.github/` does not exist. Every gate in this report was executed by hand on one Windows machine. | **High** | `Test-Path .github` → `False` | Open |
| **F3** | **Adapters are not wired to the loop.** `director-app` references neither adapter outside tests. | **High** | Phase 11 doc; verified by the test-only reference | Open |
| **F4** | **No contributor-facing files.** No CONTRIBUTING, SECURITY, CODE_OF_CONDUCT, CHANGELOG, issue or PR templates. | **High** | All paths return `False` | Open |
| **F5** | **README clone URL is wrong.** Quickstart says `git clone https://github.com/abyyxhek/orqynn.git`; the actual remote is `orqyn.git`. Line 468 of the same file has it right. | Medium | README:243 vs `git remote -v` | Open |
| **F6** | **Two divergent local clones.** `director-brain/` is one commit behind `orqyn/`. | Medium | `git log --oneline -1` in each | Open |
| **F7** | **44 untracked scratch files**, incl. `schedule.rs.bak` and `schedule.rs.original` inside `src/`, and ~25 stale cargo-output logs that misrepresent current test results. | Medium | `git status` | Open |
| **F8** | **`Blocker` and `RecentContext` unpersisted.** `RecoveryGaps::is_clean()` is permanently `false`. | Medium | Phase 11 + Phase 12 docs | Open |
| **F9** | **Command execution is agent-supplied.** `LocalExecutor` runs `Command::new(&spec.program)` where the program comes from a task's expected-output check. There is no allowlist or sandbox. Today only tests feed it. This becomes a real attack surface the moment a driver runs it. | **High (future)** | `crates/director-adapters/src/executor.rs:68` | Open, latent |
| **F10** | **`ProviderSync` repository has no caller**; five store helpers are `#![allow(dead_code)]`. | Low | Phase 5 doc | Open |
| **F11** | **Phase 8 multi-identity pool was promised and dropped** without a note. | Low | `PHASE2-ADAPTERS.md` | Open |
| **F12** | **MSRV pin is unusual.** `rust-toolchain.toml` pins `stable-x86_64-pc-windows-gnu`, which makes the *toolchain* Windows-specific even though `Cargo.toml` declares `rust-version = "1.85"` and the README claims Linux/macOS support. A contributor on Linux gets a different resolved toolchain. | Medium | `rust-toolchain.toml` vs `README.md:236` | Open |
| **F13** | **No event retention** for git observation; events accumulate unbounded. | Low | Phase 2 doc | Open |
| **F14** | **Doc staleness in a crate-level comment:** `director-app/src/lib.rs:14` still lists the six-step loop without SCHEDULE. | Trivial | file read | Open |
| **F15** | **Duplicated paragraph** in `PHASE12-CHECKPOINT-RECOVERY.md` around lines 209–212. | Trivial | doc read | Open |

### 6.3 Verification results (executed on the audited tree)

```
cargo fmt --all --check                      → exit 0, clean
cargo clippy --workspace --all-targets       → exit 0, 0 warnings
cargo build --workspace                      → exit 0
cargo test --workspace                       → 25 suites
                                                608 passed
                                                  0 failed
                                                 13 ignored
```

Toolchain: `cargo 1.98.1`, `rustc 1.98.1` on `x86_64-pc-windows-gnu`.

**Ignored tests (13) — not run in this audit.** These require live external MCP servers:

| Suite | Ignored | Requires |
|---|---:|---|
| `director-adapters/tests/aimemory_live.rs` | 8 | a running ai-memory v2.4.0 server |
| `director-adapters/tests/handoff_live.rs` | 5 | a running handoff-mcp v0.35.1 server |

The project's own documentation states these passed when last run. **That claim is carried forward as Not Verified in this audit** — reproducing it requires standing up two external servers, which was out of scope for a non-destructive pass.

### 6.4 The previously reported E2E failures — investigated and resolved

The prior session's "end-to-end failures" trace to two untracked scratch logs:

| File | Recorded result | Named failing tests |
|---|---|---|
| `test_e2e.txt` | `4 passed; 4 failed` | `replacement_agent_recovery_context`, `restart_persistence`, … |
| `full_test_output.txt` | `2 passed; 6 failed` | `replanning_unresolvable_dependency`, `restart_persistence`, … |

**Investigation result:** an earlier, larger `tests/end_to_end.rs` existed during Phase 12 work-in-progress and was later **restructured**. A filesystem search for all three test function names across `crates/**/*.rs` returns **zero matches** — none of them exist in the current tree.

The coverage they represented was **not lost, it was relocated and renamed**. The current `tests/end_to_end.rs` contains 2 tests covering the verification path and the failure/replan path. The former `runtime_recovery.rs` tests now exist under descriptive names:

- `persistent_state_survives_a_process_restart_and_the_loop_continues` ← the restart case
- `an_agent_that_vanishes_loses_its_lease_and_a_replacement_finishes_the_work` ← the replacement case
- `a_replacement_agent_can_be_given_everything_the_prior_agent_left_behind`
- `a_failed_task_can_be_reworked_for_a_replacement_agent_who_then_passes`
- `an_abandoned_process_leaves_committed_state_intact_and_recoverable`

**Conclusion: the E2E failures are stale artefacts of a superseded test layout and are resolved in the current tree.** Evidence: `cargo test --workspace` → 0 failures. **Recommendation:** delete the ~25 stale `.txt` logs so this cannot mislead a future reader again. (Not deleted here, per the preservation constraint.)

### 6.5 Security posture

| Area | Assessment |
|---|---|
| Memory safety | `director-app` declares `unsafe_code = "forbid"`. Only 13 `unsafe` occurrences workspace-wide (dependency-adjacent). Strong. |
| SQL injection | All queries parameterised; migrations are static `include_str!` files. Strong. |
| Substrate coupling | Enforced as a **build failure** by `tests/boundary.rs`. Strong — this is the control most projects only document. |
| Command execution | `LocalExecutor` spawns arbitrary programs from task-supplied checks, 60s timeout, child killed on timeout, no allowlist or sandbox. **Latent high risk** once a driver exists. |
| Path traversal | `GitObserver` rejects `..` and roots-elsewhere paths — but per `PHASE2-OBSERVATION.md`, **these rules have no direct tests**; `service.rs` cites a test file that does not exist. Weakest test coverage in a security-relevant path. |
| Credentials/secrets | No secrets, tokens, or `.env` files found. MCP stdio transport uses process pipes, not network credentials. |
| Dependency posture | 103 crates, few direct deps. `rusqlite` uses `bundled` (compiles SQLite from source — good for reproducibility, slower build). `git2` chosen over shelling out to git — reduces injection surface. `cargo audit` was **not run** (tool not verified available); CVE status is **Not Verified**. |
| Third-party licensing | `THIRD_PARTY_LICENSES.md` present; both substrates MIT; README asserts no code copied. MIT project licence declared in `Cargo.toml` and `LICENSE`. |

### 6.6 Build reproducibility

`Cargo.lock` is committed; `rust-toolchain.toml` pins a channel; MSRV `1.85` is declared; the release profile uses `lto = "thin"`, `codegen-units = 1`, `strip = "debuginfo"`. Reproducibility is **good on paper** but weakened by F12 (the toolchain pin is Windows-specific). Builds were reproducible on this machine. **CI is entirely absent**, which is the dominant gap.

---

## 7. Current Project Stage

**Answering each question directly:**

- **Latest verified completed phase:** **Phase 12** (Automatic Checkpointing and Recovery Context), commit `bda3382`, documented as complete in `docs/PHASE12-CHECKPOINT-RECOVERY.md`, and confirmed by a green 608-test run on the audited tree.
- **Current active phase:** **None.** No Phase 13+ work exists in any commit, file, or document. The project is at a clean, fully-documented boundary between phases.
- **Classification: a high-quality library-level alpha.** It is *not* a prototype (the core is finished and hardened), *not* a research implementation (it makes production-shaped engineering choices and has no open questions), *not* beta (no consumer-facing surface exists), *not* an RC, and emphatically *not* production-ready.
- **What can actually run today:** `cargo build` and `cargo test` (608 tests). The 13 live MCP adapter tests, given running handoff-mcp and ai-memory servers. **That is the complete list of things a user can do.**
- **Standalone executable / daemon / CLI / Orqynn MCP server:** **None of these exist.** Zero `[[bin]]` targets, zero examples, no daemon, no MCP server. `director-app/src/lib.rs:39-44` says so explicitly.
- **Can an external developer use it without writing internal test code?** **No.** The only working demonstration of the loop is the README example, which is compiled and pinned by `tests/readme_example.rs` — which is still a test. An external developer must add a `tests/` file or a downstream binary of their own to invoke anything. This is the single clearest blocker to adoption.
- **What prevents it being a complete usable product:** the absence of the entire outer shell. Specifically: no process boundary, no driver, no MCP surface, no CLI, no way to observe Orqyn's state without writing Rust, no CI, and no contributor onboarding.
- **Ready for public open-source development?** **Yes, conditionally** — see §8. It is already publicly available on GitHub and MIT-licensed, and it is genuinely attractive to a contributor audience because the code and reasoning quality are high. But it is **not ready to accept external contributions** until the contributor-facing files exist, and **not remotely ready for production use**.

### The three distinctions the prompt asks to keep separate

| Question | Answer |
|---|---|
| **Publicly available on GitHub?** | **Yes.** Remote `github.com/abyyxhek/orqyn`, MIT licence, README and 13 design docs published. |
| **Ready for external contributions?** | **No — Needs Work.** No CONTRIBUTING, SECURITY, CODE_OF_CONDUCT, issue/PR templates, CI, or a runnable demo. A first-time contributor has no supported entry point. |
| **Ready for production use?** | **No — categorically.** Nothing is runnable by a non-developer; there is no operator surface, no observability, and no security review of the command-execution path. |

---

## 8. Open-Source Readiness

Status key: **Ready** · **Needs Work** · **Missing** · **Not Verified**

| # | Category | Status | Notes |
|---:|---|---|---|
| 1 | Repository & README quality | **Ready** | 468 lines; project description, invariants, loop, crate hierarchy, quickstart, roadmap, docs index, attribution. Badge counts (608 tests) verified accurate. |
| 2 | License & ownership clarity | **Ready** | `LICENSE` = MIT, "Copyright (c) 2026 Orqyn contributors"; declared in `Cargo.toml` and README. Note: an **individual corporate** GitHub identity owns the remote while the copyright line says "Orqyn contributors" — worth an explicit decision on who may grant that permission. |
| 3 | Installation & quick-start | **Needs Work** | Prereqs and build steps are clear and correct, but **F5**: the clone URL is wrong, and there is nothing to run after building. |
| 4 | Working examples / reproducible demos | **Missing** | No `examples/`. The README code block is real and compiles (pinned by `tests/readme_example.rs`), but it is a test, not a runnable demo a user can execute. |
| 5 | API documentation | **Needs Work** | Exceptional *narrative* docs (rustdoc prose explains rationale and caller reaction throughout). No `#![warn(missing_docs)]`, no generated API reference, no book. |
| 6 | Architecture documentation | **Ready** | `PHASE0-FORENSICS.md` (724 lines) is an outstanding upstream audit; 13 documents totalling 3,585 lines; plus `docs/assets/banner.png`. |
| 7 | Automated CI | **Missing** | No `.github/`. F2. No automated fmt, clippy, test, MSRV, or Windows/Linux/macOS matrix. |
| 8 | Unit & integration testing | **Ready** | 608 passing, 0 failing, 25 suites, 10,868 test lines, integration-first against real stores/repos/processes. `boundary.rs` and `wiring.rs` are structural guards. |
| 9 | Live/integration coverage against substrates | **Not Verified** | 13 ignored live tests exist and are documented as passing; not reproduced here (needs live servers). |
| 10 | External contributor onboarding | **Missing** | No CONTRIBUTING; no runnable demo; the only usage path is the internal test suite. |
| 11 | Issue & PR templates | **Missing** | No `.github/` at all. |
| 12 | Code of conduct | **Missing** | Not present. |
| 13 | Security vulnerability reporting | **Missing** | No `SECURITY.md`, no disclosure channel. Meaningful given `LocalExecutor` (F9) and the absent `GitObserver` path tests. |
| 14 | Dependency health & licence compatibility | **Not Verified** | 103 crates, few direct, MIT-compatible on inspection, `Cargo.lock` committed. `cargo audit` not run — **CVE status unknown**. Recommend running it before publication. |
| 15 | Secrets / credentials / unsafe paths | **Ready (with caveat)** | No secrets or credentials anywhere. `unsafe_code = "forbid"` in `director-app`. Caveat: F9 (unsandboxed command execution) and untested `GitObserver` path validation. |
| 16 | Versioning | **Needs Work** | `version = "0.1.0"` in all four crates, consistent. But no release tags exist, and `0.1.0` understates maturity for `director-domain`/`director-store`. |
| 17 | Changelog | **Missing** | None. History must be reconstructed from 43 commit messages. |
| 18 | Release process | **Missing** | No `release.toml`, no automation, no documented process. |
| 19 | Backward compatibility policy | **Needs Work** | Pre-1.0 with no policy stated. The migration framework *does* support forward compatibility (refuses newer schemas), which is a genuine asset. |
| 20 | Known limitations & roadmap transparency | **Ready** | Outstanding. Phase docs carry explicit limitations sections; README carries a roadmap with honest `🔄 Planned` status for Phase 13. |

**Overall verdict:** **Needs Work — 6 Ready, 4 Needs Work, 7 Missing, 2 Not Verified.**

**Is the missing licence a blocker?** No — **MIT is present and unambiguous** (`LICENSE`, `Cargo.toml`, README all agree). The residual concern is only whether "Orqyn contributors" is an acceptable copyright holder given the remote sits under a corporate account. That is an ownership *decision for the author*, not a defect, and it does not block publication.

**Conditions to open for contributions** (minimum credible set): F2 (CI) + F4 (CONTRIBUTING/SECURITY/CoC/templates) + F5 (clone URL) + a runnable example. Everything else is polish.

---

## 9. Future Roadmap

### Verification of the previously reported Phase 13–22 list

Every previously reported milestone was checked against the tree.

| Claimed phase | Verified? | Reality |
|---|---|---|
| **13 — Deterministic Conflict Detection** | **Real, Planned** | Appears **only** in the README roadmap as `🔄 Planned`. Traces to Phase 0 §7.4. No code. **Verified absent.** |
| **14 — End-to-End Integration Harness** | **Not a real roadmap item** | A `tests/end_to_end.rs` **already exists** and passes. Phase 14 in this framing is **already substantially delivered**; only a *driver-level* end-to-end harness is genuinely missing. Recommend re-scoping. |
| **15 — Dependency Graph & Advanced Scheduling** | **Mostly already delivered** | `graph.rs` (330 lines, 12 tests) and Phase 7's scheduler are complete. Only *advanced* policy (weights, priority, cost) remains. **Re-scope down.** |
| **16 — Policy and Decision Engine** | **Not a real roadmap item** | `Decision`/`DecisionRepository` and REPLAN's policy already exist. What is missing is *authored* policy for decomposition and remediation. **Re-scope.** |
| **17 — Multi-Provider / Agent-Runtime Integration** | **Real and P0** | Adapters exist and are unwired (§4.8). This is the **real** remaining integration work, and it subsumes the abandoned Phase 8 multi-identity pool. |
| **18 — Daemon and Orqynn MCP Server** | **Real and P0** | Never started. The project's own Phase 0 named `director-mcp` and `director-loop` crates. **Highest-value single milestone.** |
| **19 — CLI and Developer Workflow** | **Real** | Never started. |
| **20 — Observability, Metrics, Tracing** | **Real** | `tracing` is a declared dependency of `store` and `app` but I found **no `tracing` instrumentation** in the loop. Effectively unstarted. |
| **21 — API / Dashboard** | **Deferred (P3)** | **Not justified yet.** Recommended to defer until 18/19 exist. Building a dashboard before there is a process to observe is speculative. |
| **22 — Security, Reliability, CI, Deployment Hardening** | **Real and P0** | CI alone (F2) is a hard blocker for external contributions. |

**Missing from the previously reported list** (identified from the actual tree): *driver process* (arguably part of 18 but deserves its own milestone); *decomposition* (PLAN deciding what the tasks are — named by `AUDIT-2026-10-04.md` §7 and never scheduled); *persisting `Blocker`/`RecentContext`*; *removing the toolchain/`.gitattributes` line-ending hazard* (F12 — real: the build emitted `LF will be replaced by CRLF` warnings on README and test fixtures).

### Minimum credible public release

**M1 → M4 are the minimum.** M5–M7 are strongly recommended before inviting contributors. M8+ are post-release.

---

### M1 — CI Gates *(P0 · Small)*
**Goal:** make the project's own standard automatic. The project has run `fmt`, `clippy`, and 608 tests manually for weeks; none of it is enforced.
**Tasks:** `.github/workflows/ci.yml`; run `cargo fmt --check`, `cargo clippy --all-targets -D warnings`, `cargo test --workspace`; matrix on stable Linux/macOS/Windows; separate `cargo build` job for MSRV 1.85; cache `CARGO_TARGET_DIR`.
**Depends on:** nothing. **Risk:** the OneDrive/CRLF hazard may surface on Linux CI (F12) — expect first-run failures; that is the point of adding CI.
**Acceptance:** badge in README; a deliberately broken test fails the PR.
**Tests:** CI *is* the test. **Complexity:** Small.

### M2 — Contributor & Governance Files *(P0 · Small)*
**Goal:** make the repository safe to receive outside contributions.
**Tasks:** `CONTRIBUTING.md` (build/test/gate commands, commit conventions, the `boundary.rs`/`wiring.rs` rules a contributor must not break); `SECURITY.md` (disclosure channel — required given F9); `CODE_OF_CONDUCT.md` (Contributor Covenant); `.github/ISSUE_TEMPLATE/` (bug + feature); `PULL_REQUEST_TEMPLATE.md`; `CHANGELOG.md` seeded from the 43 commits; decide and record the copyright holder.
**Depends on:** nothing. **Risk:** low. **Acceptance:** every file present and linked from README.
**Complexity:** Small.

### M3 — Repository Hygiene *(P0 · Small)*
**Goal:** stop the tree from misleading readers.
**Tasks:** fix the clone URL (F5); delete the stale cargo-output `.txt` logs and `schedule.rs.bak`/`.original` (F7); add `.gitignore` coverage for scratch outputs; delete or clearly mark the `director-brain` clone (F6); fix the six-step loop comment in `director-app/src/lib.rs:14` (F14); dedupe the paragraph in `PHASE12` (F15).
**Depends on:** nothing. **Risk:** low — but **requires the author's explicit go-ahead before deleting anything**; the preservation constraint applied to this audit means nothing was removed.
**Acceptance:** fresh clone contains no misleading artefacts. **Complexity:** Small.

### M4 — A Runnable Example *(P0 · Medium)*
**Goal:** let an external developer see the loop run without writing a test.
**Tasks:** add `crates/director-app/examples/` with (a) `happy_path.rs` — PLAN → SCHEDULE → ASSIGN → MONITOR → VERIFY against a temp repo and temp DB, printing each transition; (b) `recovery_demo.rs` — kill an agent mid-task, show lease expiry and a replacement briefing. Promote the README example to point at these. Fix the README narrative that currently offers only a test-backed snippet.
**Depends on:** nothing (the API already supports it). **Risk:** low. **Acceptance:** `cargo run --example happy_path` prints a completed task and its verification record on a clean checkout.
**Tests:** examples are compiled by CI; add a smoke test asserting the example's final state.
**Complexity:** Medium.

### M5 — Deterministic Conflict Detection (Phase 13) *(P1 · Medium)*
**Goal:** make file-scope collisions *detectable*, not merely hoped away — the substantive promise of Phase 13.
**Tasks:** path-overlap analysis over task `scope_paths` (normalise separators, handle prefix-vs-subtree, glob matching); detect and refuse/dispatch pairs whose scopes intersect; expose overlap as a first-class result in the SCHEDULE report (as a `Refused`-style fact, consistent with the existing design philosophy — **not** an error); surface the downstream cascade that REPLAN already reports but does not write; decide the interaction with handoff-mcp's *advisory* overlap warnings, which warn but never block.
**Depends on:** Phase 7 (done). **Note:** this is file/worktree conflict detection, **distinct** from the store-level optimistic concurrency Orqyn already has.
**Risk:** prefix/glob matching is easy to get subtly wrong; false positives block legitimate parallelism.
**Acceptance:** two tasks with disjoint scopes schedule freely; two with overlapping scopes are detected and reported with the intersecting paths named; unit tests cover prefix, glob, and normalisation cases.
**Complexity:** Medium.

### M6 — Driver Process and Orqynn MCP Server (Phase 18) *(P0 · Large)*
**Goal:** **the single highest-value milestone.** Turn the library into a running system.
**Tasks:** a `director-app` binary tick loop (OBSERVE → PLAN-decomposition-boundary → SCHEDULE → ASSIGN → MONITOR → VERIFY → REPLAN on an interval); graceful shutdown; expose the loop's public API over MCP (the tool surface Phase 0 named `director-mcp`); **this is also where the abandoned Phase 8 multi-identity pool must be resolved** so Orqyn can drive several agents through handoff-mcp; wire `HandoffAdapter`/`AiMemoryAdapter` into the loop so they stop being a callerless surface; config file for projects, agents, leases, tick interval.
**Depends on:** M1 (CI) and M4 (example) ideally first. **Risk:** highest in the roadmap — this introduces a process boundary, a network/IPC surface, real credentials paths, and real command execution (F9). **Requires a security review before merge.**
**Acceptance:** `orqyn run --project <path>` runs the loop unattended; an MCP client can call the loop's steps; **the SIGKILL test Phase 11 could not write becomes possible and is written**; adapters are called by the loop, not only by tests.
**Tests:** driver integration tests; the kill test; adapter-in-loop tests; graceful-shutdown tests.
**Complexity:** Large.

### M7 — Decomposition Engine *(P1 · Large)*
**Goal:** close the last gap named by `AUDIT-2026-10-04.md` §7 — PLAN currently *validates a caller's* decomposition rather than *deciding* what the tasks are. This is the "Director Brain" promise in its strongest form.
**Tasks:** objective → task decomposition behind a `Decomposer` trait (so a caller or an LLM-backed implementation can supply it without coupling the core to a vendor — honouring Phase 0's architecture rule); graph construction and sizing; decomposition must remain deterministic and auditable, since everything downstream depends on it.
**Depends on:** M6 (needs a driver to run it). **Risk:** a non-deterministic decomposer would undermine the project's core guarantee that decisions are traceable. **Keep the trait boundary clean.**
**Acceptance:** PLAN produces a valid, acyclic, dependency-ordered plan from an objective alone, with the decomposition recorded in the `decisions` table; a second identical input yields an equivalent plan.
**Complexity:** Large.

### M8 — Security, Reliability & Deployment Hardening (Phase 22) *(P1 · Medium)*
**Goal:** make the runnable system safe to point at a real repository.
**Tasks:** **command-execution allowlisting/sandboxing for `LocalExecutor` (F9) — treat as a hard requirement before M6 ships publicly**; direct tests for `GitObserver` path validation (reject `..`, reject rooted-elsewhere) — flagged as a gap by Phase 2 and still open; `OBSERVE` behind the `ProjectStateProvider` trait instead of concrete `GitService`; real SIGKILL test; event-retention policy for the git observation store; persist `Blocker` and `RecentContext` so `RecoveryGaps::is_clean()` can become `true`; give `ProviderSync` a caller; remove the five `#![allow(dead_code)]` helpers or delete them.
**Depends on:** M6. **Risk:** moderate. **Acceptance:** no `unsafe`-adjacent or unsandboxed execution path reachable from an untrusted task spec; `is_clean()` returns `true`; retention bounded.
**Complexity:** Medium.

### M9 — Observability (Phase 20) *(P1 · Medium)*
**Goal:** make a running Orqyn diagnosable. `tracing` is already a dependency of `store` and `app` but carries **no instrumentation** — this is effectively starting from zero.
**Tasks:** structured spans per tick and per step; metrics — loop ticks, assignments, verifications by verdict, lease expiries, replans, conflicts, round duration; export via Prometheus or OpenTelemetry; `/health` and a state-introspection surface.
**Depends on:** M6 (needs a process to observe). **Risk:** low. **Acceptance:** an operator can answer "is it working and why is task X stuck?" without a debugger.
**Complexity:** Medium.

### M10 — CLI and Developer Workflow (Phase 19) *(P1 · Medium)*
**Goal:** the operator interface.
**Tasks:** `orqyn init / observe / plan / schedule / assign / status / verify / replan / recover / checkpoint / migrate / doctor`; machine-readable output (`--json`) for scripting and for the Phase 14 harness; human-readable progress rendering; exit codes suited to CI.
**Depends on:** M6 (CLI drives the running system) or can be built directly against the library in parallel.
**Risk:** low. **Acceptance:** `orqyn status` shows project state without a debugger; `orqyn doctor` reports schema/toolchain/substrate health.
**Complexity:** Medium.

### M11 — Driver-Level End-to-End Harness (Phase 14, re-scoped) *(P2 · Medium)*
**Goal:** the harness Phase 14 originally asked for, built against the driver rather than the library.
**Tasks:** bring up ephemeral handoff-mcp and ai-memory servers in CI; run the **full** loop including real MCP adapter calls and a real agent stub against a fixture repository; deterministic seeding of git state; the golden-transcript assertion that ties the whole system together.
**Depends on:** M1, M6. **Risk:** flakiness in CI is the main threat; budget for retry/seeding discipline.
**Acceptance:** one command reproduces a full plan-to-done run with the durable record, including a real MCP round trip, on every CI platform.
**Complexity:** Medium.

### M12 — Advanced Scheduling Policy (Phase 15, re-scoped) *(P2 · Medium)*
**Goal:** beyond the current four rules.
**Tasks:** priority/urgency signals (carefully — Phase 7 rejected a second priority system for good reason; any addition must reconcile with plan order); agent skill/throughput scoring; starvation prevention; load balancing across many agents.
**Depends on:** M5. **Risk:** a second priority model reintroducing the disagreement Phase 7 explicitly avoided. **Keep plan order authoritative.**
**Acceptance:** measurable improvement on a fixture workload with no regression in the existing four rules' tests.
**Complexity:** Medium.

### M13 — Evidence & Probe Expansion *(P2 · Medium)*
**Goal:** finish the verification engine.
**Tasks:** implement `Probe::TestSuite`, `Probe::Diff`, `Probe::File` (each is *"adding an arm to `run_probe`, and nothing else"* per Phase 10 — genuinely small per probe); evidence interpretation beyond precedence; structured evidence capture.
**Depends on:** nothing structural. **Risk:** low.
**Acceptance:** the three modelled probe kinds execute rather than answering `Unverifiable`.
**Complexity:** Medium. *(Individually this is closer to Small; Medium reflects the evidence-interpretation half.)*

### M14 — Toolchain & Platform Portability *(P2 · Small)*
**Goal:** remove the contributor-hostile edges.
**Tasks:** make `rust-toolchain.toml` portable or document the Windows pin explicitly (F12); reconcile `.gitattributes` with the LF/CRLF warnings seen in the build; verify MSRV 1.85 in CI.
**Depends on:** M1. **Risk:** low. **Acceptance:** a Linux contributor's build is warning-free.
**Complexity:** Small.

### M15 — API / Dashboard (Phase 21) *(P3 · Large — **deferred**)*
**Goal:** a web view of plans, tasks, agents, verifications, and replan history.
**Recommendation: defer.** It has no value before M6, M9, and M10 exist, and would consume the largest remaining budget for the least differentiating feature. Revisit only after M9 provides metrics and M10 provides a CLI whose UX can be validated first.
**Risk of doing it now:** high opportunity cost; a dashboard over a library with no process is unbuildable in any useful form.
**Complexity:** Large.

### Roadmap summary

| ID | Milestone | Phase | Priority | Complexity | Depends on |
|---|---|---:|:---:|---|---|
| M1 | CI Gates | 22 | **P0** | Small | — |
| M2 | Contributor & Governance Files | — | **P0** | Small | — |
| M3 | Repository Hygiene | — | **P0** | Small | — |
| M4 | Runnable Example | — | **P0** | Medium | — |
| M6 | Driver + Orqynn MCP Server | 18 | **P0** | Large | M1, M4 |
| M5 | Deterministic Conflict Detection | 13 | P1 | Medium | — |
| M7 | Decomposition Engine | — | P1 | Large | M6 |
| M8 | Security & Reliability Hardening | 22 | P1 | Medium | M6 |
| M9 | Observability | 20 | P1 | Medium | M6 |
| M10 | CLI | 19 | P1 | Medium | M6 |
| M11 | Driver-Level E2E Harness | 14 | P2 | Medium | M1, M6 |
| M12 | Advanced Scheduling | 15 | P2 | Medium | M5 |
| M13 | Evidence & Probe Expansion | 10+ | P2 | Medium | — |
| M14 | Toolchain Portability | — | P2 | Small | M1 |
| M15 | API / Dashboard | 21 | P3 | Large | M9, M10 |

**15 milestones total.** Ordering M6 last among the P0s is deliberate: it is the highest-value item but also the highest-risk, and shipping it before CI, governance files, a runnable example, and the command-execution hardening of M8 would be the wrong sequence.

---

## 10. Recommended Immediate Actions

**This week (all low-risk, all unblocking):**
1. Add CI (M1). One file. Immediately protects every future change.
2. Add `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`, PR/issue templates (M2).
3. Fix the README clone URL (F5); delete the stale `.txt` logs and `.bak`/`.original` files (F7) — **with your go-ahead**; delete or mark the `director-brain` clone (F6).
4. Add a runnable `examples/happy_path.rs` (M4). This is what turns "a library" into "something a reviewer can run in 60 seconds."

**Next sprint:**
5. Sandbox/allowlist `LocalExecutor` (F9) **before** any process runs it.
6. Add the missing `GitObserver` path-validation tests — a self-documented security gap since Phase 2.
7. Decide the copyright-holder question and run `cargo audit` (closes item 14).

**Then the big one:**
8. Scope M6 (driver + MCP server) as a focused project. It is the phase that turns Orqynn from an impressive codebase into a product — and it is what makes Phase 13, 15, 16, 17, 19, 20 and 22 all become reachable.

---

## 11. Reproducible Commands

All executed on the audited tree; results are as reported in §6.3.

```bash
# ---- Environment (verified) ----
cargo --version     # cargo 1.98.1 (797e8a9bc 2026-08-05)
rustc --version     # rustc 1.98.1 (48a229cea 2026-09-01)
# target triple: x86_64-pc-windows-gnu

# ---- Repository state ----
git -C orqyn remote -v
git -C orqyn rev-parse --abbrev-ref HEAD      # main
git -C orqyn log --oneline -1                 # 65cfa02
git -C orqyn rev-list --left-right --count origin/main...main   # 0  0
git -C orqyn status --short

# ---- Gates (all verified clean) ----
cargo fmt --all --check
cargo clippy --workspace --all-targets
cargo build --workspace
cargo test --workspace
# -> 25 suites; 608 passed; 0 failed; 13 ignored

# ---- Live substrate tests (NOT run in this audit; require live servers) ----
cargo test -p director-adapters --test handoff_live -- --ignored   # 5 tests, needs handoff-mcp v0.35.1
cargo test -p director-adapters --test aimemory_live -- --ignored  # 8 tests, needs ai-memory v2.4.0

# ---- Dependency audit (recommended, not run) ----
cargo install cargo-audit && cargo audit

# ---- Windows/OneDrive build workaround (from the README) ----
$env:CARGO_TARGET_DIR = "$HOME\.orqyn-target"
cargo test --workspace
```

---

## 12. Evidence Index

### Repository facts
`git remote -v` · `git rev-list --left-right --count origin/main...main` → `0 0` · `git log --oneline` → 43 commits, 2026-09-25 → 2026-10-14 · `git status --short` → clean tracked, 44 untracked.

### Source
| Claim | File:line |
|---|---|
| Loop is a library, "the tests are the caller" | `crates/director-app/src/lib.rs:39-44` |
| Loop diagram (six-step, pre-SCHEDULE) | `crates/director-app/src/lib.rs:14` |
| `ObserveError` incl. `Checkpointing` variant | `crates/director-app/src/lib.rs:76-93` |
| `AssignError::TaskAlreadyAssigned` refusal | `crates/director-app/src/lib.rs:264-271` |
| `CheckpointError::Conflict` | `crates/director-app/src/lib.rs:513-531` |
| Migrations registered contiguously 1..N | `crates/director-store/src/migrations.rs:63-90` |
| Failed migration leaves no tracking row | `crates/director-store/src/migrations.rs:425` |
| Newer-than-build database refused | `crates/director-store/src/migrations.rs:449` |
| Command execution entry point | `crates/director-adapters/src/executor.rs:68` |
| Default command timeout (60 s) + kill-on-timeout | `crates/director-adapters/src/executor.rs:24,104-108` |

### Tests (all executed)
`crates/director-app/tests/end_to_end.rs` (2) · `runtime_recovery.rs` (5) · `recovery_builder.rs` (18) · `replan.rs` (21) · `engine.rs` (15) · `monitor.rs` (19) · `schedule.rs` (11) · `verify.rs` (13) · `assign.rs` (13) · `observe.rs` (6) · `checkpoint_lineage.rs` (1) · `readme_example.rs` (1) · `director-store/tests/store.rs` (69) · `director-adapters/tests/boundary.rs` (3) · `wiring.rs` (1) · `aimemory_live.rs` (8 ignored) · `handoff_live.rs` (5 ignored).

### Commits (43 total; key ones)
`2502a92` Phase 0-1 forensics + domain · `f5dd9b6`/`80e24ed` Phase 2 · `4790ed3` Phase 2 handoff adapter · `1218d70` Phase 3 · `f80184e` Phase 5 store · `69258fa` Phase 6 OBSERVE · `3cd4c06` Phase 6 PLAN · `b853602` ASSIGN · `405fa9b` MONITOR · `3459c33` VERIFY · `32b7f87` REPLAN · `2e1f4fa` Phase 7 scheduler · `ad9dead` Phase 10 record layer · `2d7b213` Phase 10 engine · `373cfc4` Phase 11 validation · `bda3382` Phase 12 checkpointing · `65cfa02` README sync + pinned example.

### Documentation
`docs/PHASE0-FORENSICS.md` · `PHASE1-DOMAIN.md` · `PHASE2-ADAPTERS.md` · `PHASE2-OBSERVATION.md` · `PHASE3-MEMORY.md` · `PHASE5-STORE.md` · `PHASE6-APP.md` · `PHASE7-SCHEDULE.md` · `PHASE10-ENGINE.md` · `PHASE11-RUNTIME-RECOVERY.md` · `PHASE12-CHECKPOINT-RECOVERY.md` · `AUDIT-2026-09-30.md` · `AUDIT-2026-10-04.md` · `THIRD_PARTY_LICENSES.md` · `LICENSE` · `README.md`.

### Explicitly unverified
Live MCP adapter tests (13 ignored — need live servers) · `cargo audit` CVE status (tool not run) · Behaviour on Linux/macOS (single Windows host) · Release/tagging process (none exists) · Legal ownership beyond the declared MIT text.

---

*Report generated 2026-10-16 by automated repository audit. All quantitative claims were produced by executing the stated commands against commit `65cfa02`; no figure is carried forward on trust. The repository was not modified: no commit, push, reset, clean, or stash was performed, and no source file was edited.*