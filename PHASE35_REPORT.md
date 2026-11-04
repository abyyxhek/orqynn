# Phase 35 — Orqynn Live-Agent Integration & Daemon Readiness Report

**Repository:** https://github.com/abyyxhek/orqynn.git  
**Baseline:** `main` at `ffe96e4` (confirmed synchronized with `origin/main`)  
**Working tree:** 1 modified file (`crates/director-app/src/engine.rs`) — preserved, not committed  
**Untracked:** `PHASE33_REPORT.md`, `PHASE34_REPORT.md`  
**Date:** 2026-10-20

---

## 1. Verified Repository State and Working-Tree Differences

| Property | Value |
|----------|-------|
| Git root | `C:\Users\ASUS\OneDrive\Desktop\ai_memory\orqyn` |
| Branch / HEAD | `main` @ `ffe96e4` |
| Upstream | `origin/main` (up to date) |
| Tracked modifications | 1 file: `crates/director-app/src/engine.rs` (Phase 33 verification ID retry fix) |
| Untracked files | `PHASE33_REPORT.md`, `PHASE34_REPORT.md` |
| `cargo fmt --all -- --check` | **PASS** (exit 0) |
| `cargo clippy --workspace --all-targets -- -D warnings` | **PASS** (exit 0, 0 warnings) |
| `cargo test --workspace` | **PASS** (~655 tests, 0 failures) |

No commits, pushes, rebases, resets, or cleans performed. All user changes preserved.

---

## 2. Phase 34 Concurrency Regression Results

**All focused concurrency tests pass deterministically:**

| Test Suite | Tests | Status |
|------------|-------|--------|
| `assign_concurrency.rs` | 13 | ✅ All pass |
| `conflict.rs` | 14 | ✅ All pass |
| `process_kill.rs` | 11 (+1 ignored harness) | ✅ All pass |

**Fix confirmed in codebase:**
- `assign_and_start_resolved` (lines 779-810): `BEGIN IMMEDIATE` + in-transaction `guard_scope`
- `assign_task` (lines 586-596): `BEGIN IMMEDIATE` + in-transaction `guard_scope`
- Both share `guard_scope` (lines 505-531) — one rule, one implementation
- Failed operations leave **no partial state** (task stays `todo`, no assignment row)

---

## 3. Gateway, Model, and Authentication Status

### Current Configuration Mismatch

| Property | Configured/Documented | Live Test Override | Status |
|----------|----------------------|-------------------|--------|
| Gateway URL | `https://discovery-api.intern-ai.org.cn` | — | **Unconfirmed endpoint** |
| Historical doc reference | `https://api.atria-asi.ai` | — | Mismatch noted |
| Model | `Atria-Dawn-Preview` | `Atria-Dawn` (`claude_code_live.rs:57`) | **Mismatch** |
| Auth | None in repo | Requires authenticated session | **Blocked** |

### Live Test Requirements (`claude_code_live.rs`)

- `CLAUDE_CODE_BINARY` env var or `claude` on PATH
- Working authenticated session (Claude Code reads own credentials)
- Network access to the API endpoint
- Real token spend (capped by `--max-budget-usd`)

### Policy Enforced

- No gateway settings modified
- No credentials requested, printed, or committed
- Live test remains `#[ignore]`d
- No retry of live tests without operator authorization

**Status: Live-agent validation NOT performed — externally blocked.**

---

## 4. Focused Live-Test Results

**Not executed.** The authentication gate (Section 3) was not cleared by the authorized operator. Per Phase 35 instructions, no live test was run without explicit confirmation of endpoint, model, and credential installation.

---

## 5. Real Task Lifecycle and Independent Verification

**Not executed.** End-to-end real task execution (`PLAN → SCHEDULE → ASSIGN → DRIVE_AGENT → VERIFY → REPLAN`) requires a valid live agent, which is blocked on authentication.

**Offline validation confirmed:**
- Full lifecycle tested via deterministic fake agent fixture (`orqynn/tests/cli.rs`)
- Verification engine runs real commands (`LocalExecutor`) against repository
- Agent exit code never decides success — only verification verdict does
- Failed verification preserves correct non-success state (`TaskStatus::Failed` or `VerificationPending`)
- Exit codes and diagnostics are distinct and stable
- State survives store reopen and recovery context assembles correctly
- Retry of same `--task-id` refuses rather than duplicating plan

---

## 6. Recovery and Handoff Validation

**Not executed with live agent.** Offline recovery/handoff tests pass:

| Area | Test Evidence | Status |
|------|---------------|--------|
| Process interruption | `process_kill.rs` (5 kill points, cross-process recovery) | ✅ Offline |
| Atomic verification + actions | `verification_atomicity.rs` (9 tests) | ✅ Offline |
| Handoff claim-once & provenance | `handoffs.rs` (9 tests) | ✅ Offline |
| Handoff across reopen | `displacement_handoff.rs`, `displacement_kill.rs` | ✅ Offline |
| Recovery from persisted state | `recovery_builder.rs` (34 tests), `runtime_recovery.rs` (5 tests) | ✅ Offline |

**Key offline guarantees verified:**
- A killed process leaves committed state intact and recoverable
- An agent report alone cannot become verified completion
- A replacement process can pick up and verify to `done`
- Verdict and actions commit atomically (both-or-neither)
- Recovery reports no gap for atomically committed verification

**Power-loss durability:** Not claimed. `synchronous=NORMAL` — OS-level durability not guaranteed (documented in `process_kill.rs:81-85`).

---

## 7. Daemon and Autonomous Runtime Readiness

### Current State: **One-Shot CLI Only**

`orqynn` is a **single-lifecycle CLI** (`run` or `resume` subcommand). It opens the store, executes one complete loop iteration (or recovery), prints a report, and exits. No autonomous loop exists.

### Capability Assessment

| Capability | Status | Location |
|------------|--------|----------|
| Repeated observe–plan–schedule–execute–verify cycles | ❌ Missing | No loop driver |
| Graceful shutdown and restart | ⚠️ Partial | Process-kill recovery works; no signal handling in CLI |
| Cancellation and bounded execution | ✅ Per-task | `HarnessRun::timeout`, `DriveOptions::timeout` |
| Persistent recovery after process interruption | ✅ | `process_kill.rs`, `recovery_builder.rs` |
| Prevention of duplicate task execution | ✅ | Task ID derivation, plan membership, assignment guard |
| Safe behavior when no eligible agent | ✅ | `schedule` reports `UnassignedReason::NoEligibleAgent` |
| Structured operational logs | ✅ | `RunReport::lines`, `HarnessOutcome` detail |
| Useful failure reporting | ✅ | Distinct exit codes, `RunError` variants, `classify` |

### Missing for Autonomous Operation

1. **Loop driver** — No component that repeatedly calls `observe → schedule → drive_agent → verify → replan`
2. **Agent pool management** — No heartbeat monitoring loop, no agent lifecycle supervision
3. **Continuous scheduling** — `schedule` runs once per invocation; no periodic tick
4. **Signal handling** — No graceful shutdown on SIGTERM/SIGINT in CLI
5. **Metrics/observability** — No structured logging, metrics emission, or health endpoints

### Recommendation

Autonomous operation is **not required for initial release** if the intended use case is operator-triggered task execution via CLI. If daemon operation is needed, propose **Phase 36** with explicit acceptance criteria:
- Background service with configurable tick interval
- Agent pool registration, heartbeat monitoring, stale-agent reclamation
- Graceful shutdown on signal (persist in-flight state, release leases)
- Structured logging (JSON) and optional metrics endpoint
- Integration with systemd/container orchestration

---

## 8. `director-brain/` Divergence — Separate Investigation

### Source of Reported Regression

Phase 34 report (§8) cites a "possible `Checkpoint::supersede()` regression in `director-brain/`". Investigation confirms a **deliberate schema/behavior change**, not a regression.

### Changes in `director-brain/` (vs. `orqyn` baseline `ffe96e4`)

| Component | `orqyn` (current) | `director-brain/` (diverged) |
|-----------|-------------------|------------------------------|
| `Checkpoint::supersede()` | Increments `state_version` (1→2) | **No increment** — retired checkpoint keeps original version |
| Store `create_checkpoint` | Takes `expected_current_version`; writes `lineage_version = expected + 1` | Takes `expected_current_version`; reads `stored_current_version` from DB, writes `next_version = stored + 1` |
| Superseded row UPDATE | `state_version = state_version + 1` | **No version change** — only status update |

### Expected Behavior (per `director-brain/` design)

- Current row's `state_version` = **lineage length** (1, 2, 3...)
- Retired checkpoints keep the version they were written with
- Lineage reads as `1, 2, 3` in write order
- Current row's version = total checkpoints ever written for that task

### Impact on `orqyn`

**None currently.** The `director-brain/` repository is a **separate checkout** with its own modifications. The `orqyn` codebase at `ffe96e4` retains the original versioning scheme:
- `supersede()` increments version
- Superseded row's version advances
- Version gate compares caller's expected version against stored current row's version

**Verification:** `orqyn` tests pass (14 checkpoint tests, 74 store tests, all workspace tests).

### Evidence Required for Safe Follow-Up

If merging `director-brain/` changes into `orqyn`:
1. Update `Checkpoint::supersede()` to **not** increment version
2. Update `create_checkpoint` to compute `next_version = stored_current_version + 1`
3. Update supersession UPDATE to **not** increment superseded row's version
4. Migrate existing databases (version values will change meaning)
5. Re-validate all checkpoint, recovery, and process-kill tests
6. Confirm `StateVersionConflict` gate still catches stale writers correctly

**Status: Divergence documented. No action taken in `orqyn` this phase.**

---

## 9. Remaining Production Risks and External Blockers

| Risk | Severity | Status |
|------|----------|--------|
| **Live agent execution unvalidated** | **Critical** | Blocked on gateway auth; cannot confirm driver works against real API |
| **Gateway endpoint mismatch** | **Critical** | `discovery-api.intern-ai.org.cn` vs `api.atria-asi.ai` — operator must confirm |
| **Model identifier mismatch** | **Critical** | `Atria-Dawn-Preview` vs `Atria-Dawn` — operator must confirm |
| **No credentials installed** | **Critical** | No approved local secret-management mechanism confirmed |
| Concurrent assignment race | Resolved | Fixed via `BEGIN IMMEDIATE` + in-transaction `guard_scope`; regression tests pass |
| `director-brain/` checkpoint divergence | Medium | Documented; separate repo; no impact on `orqyn` unless merged |
| Machine power loss durability | Medium | `synchronous=NORMAL`; OS-level durability not guaranteed |
| Cross-project scope comparison | Low | Not implemented; correct while project = one repository root |
| Replacement agent briefing omits scope | Low | `ContinuationPackage` lacks `scope_paths`; enforcement unaffected |

---

## 10. Prioritized Recommendation for Phase 36

### Prerequisites (Operator Action Required)

1. **Confirm gateway URL** — `discovery-api.intern-ai.org.cn` or `api.atria-asi.ai`
2. **Confirm model identifier** — `Atria-Dawn-Preview` or `Atria-Dawn`
3. **Install credentials** via approved local mechanism (env var, config file, secret manager)
4. **Authorize live test execution** — remove `#[ignore]` from `claude_code_live.rs`

### Phase 36 Options

| Option | Scope | When to Choose |
|--------|-------|----------------|
| **36A: Live Agent Validation** | Enable `claude_code_live.rs`, run focused test, execute one real task end-to-end, verify recovery | **Required** if live agent is on critical path for release |
| **36B: Autonomous Daemon** | Build loop driver, agent pool, signal handling, structured logging | Required only if product requires background autonomous operation |
| **36C: `director-brain/` Merge** | Merge checkpoint versioning changes, migrate databases, re-validate | Required only if `director-brain/` is the intended upstream |

### Recommended Priority

1. **Phase 36A** — Unblocks production validation of the core agent integration
2. **Phase 36B** — Only if autonomous operation is a release requirement
3. **Phase 36C** — Only if `director-brain/` supersession semantics are preferred

---

## Completion Summary

| Phase 35 Objective | Status |
|-------------------|--------|
| Verify repository state | ✅ Complete |
| Revalidate Phase 34 concurrency fix | ✅ Confirmed (13+14+11 tests pass) |
| Gateway/auth gate | ❌ **Blocked** — operator confirmation required |
| Focused Claude Code driver validation | ⏸ **Not executed** — blocked on auth |
| Real end-to-end task execution | ⏸ **Not executed** — blocked on auth |
| Recovery/handoff validation | ✅ **Offline complete** (27+ tests) |
| Daemon readiness assessment | ✅ **Complete** — one-shot CLI only, no autonomous loop |
| `director-brain/` divergence investigation | ✅ **Complete** — documented, no action needed |
| Full validation (fmt/clippy/test) | ✅ **Pass** (~655 tests) |

**Phase 35 partially complete.** Offline work is complete and verified. Live-agent validation remains externally blocked pending operator authorization. All changes preserved for review.

---

**Report complete.** No commits or pushes made. Working tree unchanged from baseline.