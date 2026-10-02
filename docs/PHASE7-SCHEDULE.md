# Phase 7 — The Scheduler

> **Status: complete.** The `director-app` crate's seventh step is landed and
> tested: SCHEDULE decides which ready task goes to which available agent, and
> a loop tick can hand out work without somebody naming every agent.

## Goal

Phase 6 landed all six steps of the loop, and every one of them deliberately
kept its judgment outside the function. PLAN takes a stated decomposition;
ASSIGN takes a named agent; REPLAN takes a remediation per task. That is what
makes each step deterministic and testable — but it also means the loop only
runs while a human answers each question as it comes up. The six steps are
library functions, and their caller is the tests.

Phase 7 removes the one judgment that does not need a human: *which* of the
ready tasks goes to *which* of the available agents. It does not invent work
(that is PLAN's caller's reasoning), it does not judge work (that is VERIFY),
and it does not decide what a failure means (that is REPLAN). It takes the work
`ready_tasks` already reports and the agents `list_agents` already returns, and
it picks a pairing.

```text
OBSERVE → PLAN → SCHEDULE → MONITOR → VERIFY → REPLAN
                     ▲
                     │
             ready_tasks          ← what can be handed out, in plan order
           + list_agents          ← who can take it
           + assignment_history   ← who has already tried
```

The step is a composition, and that is the whole point of it: it adds **no new
store method and no new domain entity**. `ready_tasks`, `list_agents`, and
`assignment_history` all exist from Phase 5 and 6, and every write goes through
`assign`, which was already tested. What is new is the policy and the report.

## SCHEDULE: ready work becomes paired work

One round of `schedule()`:

1. Reads the work — `assign::ready_tasks`, which already requires an active
   plan, already drops anything not `todo`, and already yields in the plan's
   execution order.
2. Reads the registry — `list_agents`, every agent Orqyn knows.
3. Ranks the eligible agents for each task with `match_agents`, the whole
   policy in one pure function.
4. Applies each pairing through `assign::assign`, which re-checks the handoff's
   legality against live state and does the write.
5. Reports every outcome — applied, refused, or left without an agent.

The round writes nothing itself. It owns the *choice*; ASSIGN still owns the
*legality*, and that split is why the scheduler cannot produce an illegal
assignment.

## The policy, and why it is these four rules

`match_agents` is pure — no store, no I/O, no clock — so the whole policy is
testable without a round. Eligibility first, then a sort key:

1. **Eligible.** The agent's status can accept work, it is not already showing
   a current task, and it declares every capability the task requires. Both
   halves of the first two earn their keep, and the reason is not obvious:
   `assign` moves an agent's `current_task` but leaves its status `Available`
   — `Busy` arrives only when the agent acknowledges the work through MONITOR —
   so `current_task` is what excludes an agent already holding something, and
   the status is what excludes one that has since gone quiet. Either alone
   would let a working agent through.
2. **Fresh eyes first.** Prefer an agent who has never held this task, per its
   `assignment_history`. A task that is back to `todo` is a task a tenure ended
   without finishing, and handing it to the same agent is the pairing most
   likely to repeat the outcome. In the common case this rule and rule 1 agree
   — a lease MONITOR expired leaves its holder `Disconnected`, which rule 1
   already refuses — so the rule only actually bites for an agent that
   abandoned work while still alive. It is a **fallback**, never a prohibition:
   when the prior holder is the only eligible agent, excluding them would
   strand the task forever, so the sort key de-prioritizes instead.
3. **Specialist first.** Among eligible agents, prefer the one declaring the
   *fewest* capabilities, so specialists take specialist work and generalists
   stay free for the tasks only they can cover. Ranking the other way spends
   the only agent who can do the later task on the one anybody could do.
4. **Agent id** as the final tie-break, so the same state always yields the
   same pairing and a test is stable across runs.

Task *priority* is deliberately not an axis. The plan's order is already the
priority order — `ready_in_plan` yields in it — and a second priority system
would disagree with the first.

## One task per agent per round

An agent holds one task; that is the model, and the round honors it in two
places. The eligibility filter skips an agent showing a `current_task`, and the
round also carries the set of agents it has already committed *this round*,
because the store's view has not caught up with a proposal the round itself
just made — `list_agents` was read once, at the top.

The second check is what makes two ready tasks competing for one agent
deterministic: the earlier task in plan order wins, and the later one is
reported as `AgentTakenEarlier` rather than being proposed to an agent the
round knows is taken and having the store refuse it.

## Proposals are a snapshot; the write is live

The round computes its pairings from a snapshot and applies them one at a time
through `assign`, which re-validates against the live store. A proposal
computed a moment ago can be stale by the time it is applied — another step
moved the task, an agent went quiet, the plan was superseded — and a stale
proposal is *refused*, never an illegal write.

That is the difference from REPLAN, and getting it backwards would be a real
bug. REPLAN validates every decision before applying any, because its
decisions can conflict *with each other* — two decisions about the same task,
a remediation for a task another decision cancelled. Scheduler proposals can
only conflict *through the store* — two tasks wanting one agent — and `assign`
plus the store's own invariants already guard that. Applying one at a time
means the losing task is simply reported, and nothing is applied that the
world has moved out from under.

A refused proposal is therefore not an error, for the same reason a refused
decision is not one in REPLAN: it is a fact about the world the round read,
not a failure of the round. It lands in the report with the `AssignError` that
explains it, nothing was written, and the task is still `todo` and still
handable next round.

## What the report is for

The report carries three lists — applied, refused, unassigned — and the third
is the reason the step is worth having over calling `assign` by hand. Work
that no available agent can do surfaces as `NoEligibleAgent` instead of
silently remaining `todo` round after round; work an agent could have taken
but lost to an earlier task surfaces as `AgentTakenEarlier`, which is
contention rather than a shortage. A caller that drives the loop has something
to escalate on, and a caller that does not has a list of what to fix.

`is_settled()` is true only when nothing is left waiting — a refused pairing
is not settled, because the task is still handable, and work with no eligible
agent is not settled either, because it will not clear on its own.

## What this step deliberately does not do

It does not invent tasks, create or supersede plans, judge work, or reclaim
leases — those are PLAN, PLAN, VERIFY, and MONITOR. It touches only the tasks
`ready_tasks` returns, so in-flight work is none of its business, and a task
MONITOR already reclaimed is back to `todo` with a holder who is
`Disconnected`: the two steps cannot overlap on the same task in the same tick.

Nor does it decide what a failure means. If every eligible agent has held the
task and it still comes back, that is a signal for REPLAN or a caller, not a
scheduling decision. The policy picks the best available pairing; it does not
conclude that a good pairing exists.

## What the tests prove

`match_agents` is unit-tested without a store, because the policy is the part
most likely to be subtly wrong and the cheapest to pin down:

- An agent missing a required capability is not ranked; an unavailable agent
  is not ranked; an agent showing a current task is not ranked, even if its
  status still says `Available`.
- A `Human` requirement is satisfied only by a human agent — a generalist
  coding agent does not, even though `Coding` is otherwise a
  super-capability.
- A specialist ranks above a generalist, ties break on agent id, and the same
  inputs yield the same ranking twice.
- An agent who never held the task ranks above one who did; between two prior
  holders, the one with fewer tenures wins; and a prior holder alone is still
  returned, because de-prioritizing must not strand the task.
- `unassigned_reason` distinguishes an empty registry from a capability
  shortage from contention, because a caller reacts to each differently.

`tests/schedule.rs` works against a real store, asserts by reloading, and
produces its preconditions through the loop's own steps where a loop step can
produce them:

- Every ready task goes to a distinct available agent, and both tasks actually
  moved to `in_progress` — the report's word is not the store's.
- One agent gets one task per round, and the task that lost the agent is
  reported as contention and left completely untouched: still `todo`, no
  assignment row.
- A specialist takes the specialist task and the generalist takes the rest.
- Work no available agent can do is reported as `NoEligibleAgent` with nothing
  written, and an agent that went quiet between the plan and the round is not
  handed work it cannot answer for.
- An agent who never held a task is preferred over one who released it, and a
  prior holder is still used when nobody else can take the task — the released
  tenure is retained in history, not deleted, which is what the rule reads.
- A second round over an already-scheduled project finds nothing ready and
  changes nothing, which is the round's idempotency.
- A project with no active plan is `NoActivePlan` — an error, not a refused
  pairing, because a caller has to know the difference between a store that
  failed and a project that is not executing anything.

One precondition no loop step produces yet, and the tests set it at the store
level rather than pretending otherwise: a task back to `todo` whose former
holder is still `Available`. MONITOR's lease expiry puts the task back to
`todo` but marks the holder `Disconnected`, so the "agent abandoned work while
still alive" state is reachable today only by releasing the tenure and
reopening the task directly. The tests say so where they do it.
