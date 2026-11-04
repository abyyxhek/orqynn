# Phase 33 — Orqynn Offline Reliability & Release Readiness Report

**Repository:** https://github.com/abyyxhek/orqynn.git  
**Baseline:** `main` at `ffe96e4` (confirmed synchronized with `origin/main`)  
**Working tree:** 1 modified file (`crates/director-app/src/engine.rs`) — preserved, not committed  
**Date:** 2026-10-20

---

## 1. Repository and Branch State

```
Branch: main
HEAD: ffe96e4 (docs: update README for phases 19-26 — handoffs CLI, plan guard, agent driver, MCP hardening, atomic verification, store-allocated IDs)
Upstream: origin/main (up to date)
Working tree: 1 modified file (crates/director-app/src/engine.rs)
Untracked: none
```

No commits, rebases, force-pushes, or discards performed. All user changes preserved.

---

## 2. Current Offline Test Baseline

| Command | Exit Code | Duration | Result |
|---------|-----------|----------|--------|
| `cargo fmt --all -- --check` | 0 | <1s | **PASS** (no output = clean) |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 | 15.53s | **PASS** (0 warnings) |
| `cargo test --workspace` | 0 | ~30s | **PASS** (all tests) |

### Test Totals by Crate

| Crate / Suite | Tests | Passed | Failed | Ignored |
|---------------|-------|--------|--------|---------|
| `director-domain` (unit) | 196 | 196 | 0 | 0 |
| `director-store` (unit) | 29 | 29 | 0 | 0 |
| `director-app` integration tests | ~280 | ~280 | 0 | 2 (child harnesses) |
| `director-adapters` (unit + MCP + harness) | ~60 | ~60 | 0 | 1 (claude_code_live) |
| `orqynn` CLI tests | 72 | 72 | 0 | 0 |
| Doc tests | 3 | 3 | 0 | 0 |
| **Total** | **~640** | **~640** | **0** | **3** |

*Ignored tests are harness roles (`child_harness` ×2, `claude_code_live` ×1) — not failures.*

---

## 3. Timeout / Retry Behavior Audit

### 3.1 Timeout Layers

| Layer | Timeout | Source | Notes |
|-------|---------|--------|-------|
| **Harness process (default)** | 900s (15 min) | `ClaudeCodeConfig::default_timeout` | Deliberately generous: "a measured 401 took 177s to surface, because it retries before giving up" |
| **Harness run override** | per-`HarnessRun::timeout` | `HarnessRun::new()` default 900s | `claude_code_live.rs` overrides to 300s |
| **Executor (command probes)** | 60s | `LocalExecutor::DEFAULT_TIMEOUT` | Verification engine checks only |
| **MCP transport (handshake/tool)** | sub-second | `McpTimeouts::fast()` | Fixture tests only |
| **External memory retrieval** | `EXTERNAL_MEMORY_TIMEOUT` | `DriveOptions::external_memory_timeout` | Bounded, non-blocking |

### 3.2 Retry Behavior

| Scenario | Behavior |
|----------|----------|
| **Harness timeout** | Child killed via `child.start_kill()`, reaped, returns `HarnessError::Timeout` — **no retry** |
| **Harness spawn failure** | Returns `HarnessError::Spawn` / `BinaryMissing` — **no retry** |
| **Harness reports `is_error: true`** | Captured in `HarnessOutcome.reported_error`, `terminal_reason` folded into stderr — **no retry** |
| **401 / auth failure** | Manifests as `is_error: true` + non-zero exit + `terminal_reason` — **distinguishable from timeout** (timeout = `HarnessError::Timeout`, auth = `HarnessOutcome` with `reported_error=true`) |
| **Invalid model** | Same as auth: harness-reported error via JSON result |
| **Verification ID collision** | Engine retries with fresh ID (UNIQUE constraint on `verifications.id`) |
| **Stale `state_version`** | Store refuses with `StoreError::StateVersionConflict` — caller must re-read and re-run |

### 3.3 Fast Failure & Diagnostics

- **Separate stdout/stderr capture** — diagnostics never merge into result JSON
- **`terminal_reason` folded into stderr** — machine-readable stop reason preserved as evidence
- **`HarnessOutcome` carries full detail**: `exit_code`, `stdout`, `stderr`, `truncated`, `reported_error`, `duration_ms`, `result_text`, `harness_session`, `cost_usd`
- **Live test (`claude_code_live.rs`)** panics with sanitized diagnostics on `reported_error`:
  ```rust
  panic!("the installed claude CLI reported an error... result: {}\nstderr: {}", ...)
  ```
- **No silent successes** — `HarnessOutcome::succeeded()` requires `exit_code == 0 && !truncated && !reported_error`

### 3.4 Confirmed: No Test-Harness Defects Found

All timeout/retry paths behave as designed. The generous 900s default is intentional (documented in code) to avoid misclassifying slow auth as timeout. No regression tests needed.

---

## 4. Reliability Invariants — Coverage Summary

| Invariant | Test Evidence | Status |
|-----------|---------------|--------|
| **Verification-authoritative completion** | `monitor.rs::completion::a_completion_report_awaits_verification_without_completing`, `verify.rs::a_failed_verification_leaves_the_task_failed_and_replannable`, `process_kill.rs::a_kill_after_an_agent_report_does_not_become_verified_completion` | ✅ Tested |
| **Atomic verification + action persistence** | `verification_atomicity.rs` (9 tests: success commit, failure rollback, reopen consistency, CAS, replay refusal, deterministic actions, recovery gap detection, unverifiable rounds, failing checks, multi-check ordering) | ✅ Tested |
| **Assignment eligibility & concurrency** | `schedule.rs` (11 tests), `assign_concurrency.rs`, `plan_eligibility.rs` (7 tests) | ✅ Tested |
| **Handoff claim-once & provenance** | `handoffs.rs` (9 tests: round-trip, acceptance, concurrent acceptance winner, refusals, staleness, no completion grant, identity/history, legacy migration) | ✅ Tested |
| **MCP timeout / malformed / poisoning** | `mcp_transport.rs` (13 tests: handshake timeout, tool-call timeout, early exit, malformed, JSON-RPC error, late-reply poisoning, write-failure poisoning, bounded shutdown, sequential calls, zero timeout refusal, stderr separation, stderr bounding) | ✅ Tested |
| **Process interruption & recovery** | `process_kill.rs` (11 tests + 1 ignored harness): 5 kill points (checkpoint, assignment, agent report, verdict, actions) with cross-process recovery verification | ✅ Tested |
| **DB migrations & unsupported schema** | `store.rs`: idempotent open, newer DB refused (v999), migration contiguity, fresh DB, failing migration leaves no tracking row, uniqueness constraints enforced | ✅ Tested |

### Unverified / Not Covered (Not Defects)

| Area | Reason |
|------|--------|
| **Concurrent assignment race (IMMEDIATE transaction)** | Documented as unresolved in `process_kill.rs:89-94`; tests are strictly sequential |
| **Machine power loss under `synchronous=NORMAL`** | Explicitly not guaranteed (`process_kill.rs:81-85`); OS-level durability not in scope |
| **Live agent execution (Claude Code)** | External auth blocker — offline only |
| **MCP servers (ai-memory, handoff-mcp)** | Tested via fixture only; live suites separate |

---

## 5. Confirmed Fixes & Regression Tests

**None required.** All offline validation passes. No test-harness defects were found during audit.

---

## 6. External Gateway Blocker Status

| Property | Configured Value | Live Test Override | Status |
|----------|------------------|-------------------|--------|
| Gateway URL | `https://discovery-api.intern-ai.org.cn` | — | **Blocked** (unconfirmed endpoint) |
| Historical doc reference | `https://api.atria-asi.ai` | — | Mismatch noted |
| Model | `Atria-Dawn-Preview` | `Atria-Dawn` (in `claude_code_live.rs:57`) | **Blocked** (mismatch) |
| Auth | None in repo | Requires authenticated session | **Blocked** |

**Policy enforced:**
- No gateway settings modified
- No credentials requested or installed
- Live test (`claude_code_live.rs`) remains `#[ignore]`d
- No retry of live tests
- Offline validation only

---

## 7. Release-Readiness Assessment

### ✅ Ready (Offline)
- All ~640 tests pass reproducibly
- `cargo fmt` / `cargo clippy` clean
- Reliability invariants covered by integration tests with real SQLite, real process boundaries, real child processes
- Atomic verification, handoff claim-once, MCP transport poisoning, process-kill recovery all verified
- Database migration safety (newer DB refused, migrations contiguous, idempotent open)

### ⚠️ Limitations (Explicit)
1. **Live agent execution unvalidated** — Cannot confirm Claude Code driver works against real API without authorized credentials and confirmed endpoint/model
2. **Concurrent assignment race** — Not resolved; `IMMEDIATE` transaction fix pending (doc'd in `PHASE13_AUDIT_NOTE.md`)
3. **Power-loss durability** — `synchronous=NORMAL`; not guaranteed under machine crash
4. **MCP substrate (ai-memory/handoff-mcp)** — Not exercised in this repo; fixture tests only

### 📋 Release Gate Checklist

| Gate | Status |
|------|--------|
| Offline test suite green | ✅ |
| Formatting clean | ✅ |
| Clippy clean | ✅ |
| No known test-harness defects | ✅ |
| Reliability invariants tested | ✅ |
| External auth resolved | ❌ (blocker) |
| Concurrent assignment safe | ❌ (known gap) |
| Live agent validated | ❌ (blocker) |

---

## 8. Next Recommended Phase

**Phase 34 — Live Agent Validation & Concurrent Assignment Hardening**

### Prerequisites (Operator Action Required)
1. Confirm intended gateway URL (`discovery-api.intern-ai.org.cn` vs `api.atria-asi.ai`)
2. Confirm intended model identifier (`Atria-Dawn-Preview` vs `Atria-Dawn`)
3. Provide credential installation mechanism (env var, config file, secret manager)
4. Authorize live test execution

### Scope
- Enable `claude_code_live.rs` test (remove `#[ignore]`) with verified credentials
- Implement `IMMEDIATE` transaction for concurrent assignment (resolves `process_kill.rs` documented gap)
- Validate end-to-end: plan → schedule → assign → drive_agent → verify → replan with real harness
- Consider `synchronous=FULL` for production durability if power-loss guarantee needed

### Non-Goals
- No new architectural features
- No gateway/config changes without operator authorization
- No credential handling in repository

---

**Report complete.** Offline validation is reproducible. All confirmed test-harness defects: **none**. Remaining release blockers: **external auth + concurrent assignment race**, clearly documented above.