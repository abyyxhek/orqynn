# Orqyn — Phase 42 Multi-Agent Coordination Audit

**Type:** Multi-agent coordination safety & reliability audit
**Repo:** `https://github.com/abyyxhek/orqynn.git`
**Audited tree:** `main` @ `c3243cf46988b6040fb7f0abd64fd0223bdc02f7` (plus uncommitted audit artifact: this file)
**Audit date:** 2026-11-03
**Auditor persona:** senior distributed-systems & Rust concurrency engineer

## 1. Architecture overview

Orqyn is a hexagonal Rust orchestration brain. The workspace is five crates:

| Crate | Role |
|---|---|
| `director-domain` | Pure model: entities, state machines, 16 sealed newtype IDs, provider traits, repository traits. Zero I/O. |
| `director-store` | SQLite persistence: plans/tasks/assignments/verifications/checkpoints/handoffs, migrations `0001`-`0006`, optimistic concurrency via `state_version`, `BEGIN IMMEDIATE` write transactions. |
| `director-adapters` | In-memory fixtures, local executor, git observer, agent harness driver, stdio MCP clients (handoff-mcp, ai-memory). Enforced boundary crate. |
| `director-app` | Control loop: `observe -> plan -> schedule -> assign -> monitor -> verify -> replan`, plus `checkpoint` and `recovery`. |
| `orqynn` | One-shot CLI binary: `run`, `resume`, `handoffs`. |

Key architectural facts:

- SQLite is authoritative; the markdown wiki is source of truth only inside substrates.
- Orqyn is an **MCP client** (stdio JSON-RPC) and **parent process** of the agent binary. No inbound MCP surface, one-shot CLI, no daemon.
- Append-only history: plans supersede, decisions/verifications/assignments accumulate, optimistic compare-and-set on `state_version`.
- Boundary enforcement is a build-level test (`director-adapters/tests/boundary.rs`).

## 2. Agent coordination model

- **Identity:** sealed `TaskId` / `AgentId` newtypes (16 total) cannot be coerced into one another; `Task` carries no agent field. Assignments are first-class `AgentAssignment` records.
- **Eligibility:** `schedule` uses a 4-tier policy (eligibility, fresh-eyes, specialist-first, deterministic tie-break).
- **Ownership:** one task <-> one active assignment; claim-once semantics enforced by partial unique index + re-check inside `BEGIN IMMEDIATE`.
- **Lease:** TTL + heartbeats; `monitor` reclaims expired leases, marks session `vanished`, returns task to `todo`.
- **Handoff:** typed handoff records; receive/displacement flows; claim-once handoff acceptance.
- **Recovery:** checkpoints derived in `OBSERVE` from real git change evidence; `RecoveryBuilder` assembles continuation packages; gap reporting.

## 3. Task lifecycle analysis

States observed: `todo`, `in_progress`, `verification_pending`, `failed`, `done`, `cancelled`, `blocked` (+ replan remediation: `Retry` / `Rework` / `Cancel`).

Critical invariant: `TaskStatus::Done` is **unreachable** via agent self-report — only the VERIFY engine's machine probes move a task to done.

## 4. Concurrency test scenarios

| # | Scenario | Required property | Covering offline tests |
|---|---|---|---|
| 1 | Two agents claim the same task simultaneously | No unauthorized duplicate claims | `director-app/tests/assign_concurrency.rs`, `director-adapters/tests/conflict.rs`, `director-store/tests/assignments.rs` |
| 2 | One agent crashes after claiming | No lost committed state | `director-app/tests/process_kill.rs`, `director-store/tests/displacement_kill.rs`, `director-app/tests/runtime_recovery.rs` |
| 3 | Lease expires while agent works | Stale agent cannot overwrite newer state | `director-app/tests/monitor.rs`, `director-app/tests/schedule.rs` |
| 4 | Agent submits result after lease expiry | Invalid transition refused / recovery | `director-app/tests/replan.rs`, `director-app/tests/monitor.rs` |
| 5 | Old agent submits update after another takes ownership | No ownership bypass / stale overwrite | `director-adapters/tests/conflict.rs`, `director-app/tests/handoffs.rs`, `director-app/tests/receive_handoff.rs`, `director-store/tests/displacement_handoff.rs` |
| 6 | Two agents update the same task version | No lost concurrent writes / CAS | `director-app/tests/verification_atomicity.rs`, `director-app/tests/action_ordering.rs`, `director-store/tests/store.rs` |
| 7 | Handoff submitted twice | No duplicate handoff claim | `director-app/tests/handoffs.rs`, `director-app/tests/receive_handoff.rs`, `director-store/tests/displacement_handoff.rs` |
| 8 | Task recovered after process restart | No lost committed state | `director-app/tests/runtime_recovery.rs`, `director-app/tests/process_kill.rs`, `director-store/tests/displacement_kill.rs` |
| 9 | One agent completes while another attempts to claim | No ownership bypass | `director-adapters/tests/conflict.rs`, `director-app/tests/handoffs.rs`, `director-app/tests/end_to_end.rs` |
| 10 | Multiple independent tasks assigned concurrently | Correct behavior under SQLite contention | `director-app/tests/assign_concurrency.rs`, `director-app/tests/end_to_end.rs` |

## 5. Test execution results

Offline gates executed during this audit (no live-agent integrations run):

- Full workspace: `cargo test --workspace --no-fail-fast` -> **1,051 passed / 0 failed / 19 ignored** (56 suites).
- Coordination-scoped: `cargo test -p director-app -p director-store -p director-domain -p director-adapters --no-fail-fast` -> **1,009 passed / 0 failed / 19 ignored** (54 suites).
- `cargo fmt --all -- --check` -> clean.
- `cargo clippy --workspace --all-targets -- -D warnings` -> clean.
- `git diff --check` -> clean.

Per-scenario offline results: all ten scenarios above are covered by at least one executed offline suite and all executed suites passed with zero failures.

## 6. Confirmed defects

1. Only `Probe::Command` is implemented; `TestSuite`, `Diff`, `File` probe kinds are modelled and answered `Unverifiable`.
2. `RecoveryGaps::structural()` sets both gap flags unconditionally, so `Recovery::is_clean()` is `false` and `actions_available()` is `false` for every build; `Recovery::actions_complete()` returns `false` for every build.
3. `Blocker` is **not persisted** (no table, migration, or repository).
4. `ProviderSync` is never written.
5. `OBSERVE` takes a concrete `GitService`, not the `ProjectStateProvider` trait.
6. MCP transport failure behaviour is tested at the adapter, never inside the loop.
7. Feature-matrix rows for MCP server, daemon, multi-identity pool, objective decomposition, retries remain unimplemented.

## 7. Potential race conditions

- **Fixed during Phase 34:** scope-conflict check previously ran partly outside the transaction; now the authoritative scope guard re-runs inside `BEGIN IMMEDIATE` (`assign_and_start_resolved`, `assign_task`), sharing one `guard_scope` implementation.
- Remaining: process-level concurrency (two Orqyn processes against one SQLite) is exercised at the store layer; cross-process simultaneous claims are covered by ID/allocation suites but full multi-process contention behaviour is not live-verified.

## 8. Missing tests

- Live multi-agent behaviour (real concurrent agents, real crashes, real leases) is out of scope and behind `#[ignore]`.
- No persisted `Blocker` lifecycle coverage at store level.
- No `ProviderSync` write coverage.
- True multi-process simultaneous-claim stress (beyond in-process/concurrency-unit level) is limited.

## 9. Recommended fixes

1. Persist `Blocker` (table/migration/repository) and wire it into recovery.
2. Implement `ProviderSync` persistence.
3. Extend `OBSERVE` to code against the provider trait.
4. Add loop-level MCP failure injection tests.
5. Implement probe kinds beyond `Command` when machine-checkable.
6. Add multi-process contention stress tests.

## 10. Final reliability verdict

**Offline-verified as green; live-unvalidated.** Multi-agent coordination ownership, lease, handoff, recovery, and CAS invariants hold in the executed offline suites with zero failures. No critical ownership/concurrency defect was found in the exercised paths during this audit; risky live testing was not performed per safety rules.

## Next recommended phase

**Phase 35 (live-agent integration validation) — run the previously-ignored live substrate and authenticated agent suites (`handoff_live`, `aimemory_live`, `claude_code_live`), then close the remaining coordination gaps (Blocker persistence, ProviderSync) before any multi-agent scale-out.**

**Exact reason for selection:** all ten mandated coordination scenarios are offline-green, but multi-agent safety ultimately depends on real concurrent agents, real process crashes, and real lease expiry — behaviour that only the live path can exercise; the current reliability verdict is therefore limited to "offline-verified, live-unvalidated".
