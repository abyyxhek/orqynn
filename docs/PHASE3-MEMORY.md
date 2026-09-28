# Phase 3 — The ai-memory Adapter: Long-Term Memory Made Real

> Orqyn · Phase 3 deliverable
> Date: 2026-09-27
> Scope: `director-adapters` only. One new substrate adapter, plus one change to
> a `director-domain` type. No loop, no persistence of Orqyn-owned entities.

## Goal

Give Phase 1's `MemoryProvider` a second implementor that is not a `HashMap`.

Phase 2 made the task, agent, and session halves of the boundary real against
handoff-mcp. The memory half was still the in-memory fake: `MemoryProvider` had
one implementor, which proved the trait was usable and nothing else. Phase 3
wires it to the substrate that actually owns the problem — ai-memory, whose
FTS5 + entity + graph retrieval with decay is already solved well, and which
the Phase 1 trait doc identifies as the one place Orqyn deliberately
*reuses* a substrate rather than reimplements one.

Three modules in `crates/director-adapters/src/aimemory/`, mirroring the
handoff adapter's shape so the two substrates read as one pattern:

| Module | Job |
|---|---|
| `wire` | Serde mirrors of ai-memory's JSON replies, verified against a running server. Not the domain types and unable to become them. |
| `transport` | A thin wrapper over the shared stdio transport, owning the spawn shape and the data directory. |
| `adapter` | [`AiMemoryAdapter`] — `MemoryProvider` made real by composing the two above, plus the mapping neither can carry. |

[`AiMemoryAdapter`]: ../crates/director-adapters/src/aimemory/adapter.rs

## Four places the models do not line up

ai-memory's model is a wiki of markdown pages; Orqyn's is a flat record with
an id, a title, a body, a kind, tags, a score, and a change time. Four
differences are load-bearing, and each is handled explicitly rather than
coerced.

### 1. Scoping: Orqyn is a static MCP client

ai-memory routes project scope from explicit `workspace` + `project` arguments,
a `.ai-memory.toml` marker in the caller's working directory, or an
active-project pointer keyed by *session* identity. The third is unavailable to
Orqyn: it spawns a child and speaks JSON-RPC, and no lifecycle-hook session
id is bridged onto those requests.

So the adapter sends `workspace` and `project` on **every** call. This is not
just correctness, it is a property worth having: an explicit pair pins a read to
exactly one project and makes the fallback path unreachable. The substrate
itself requires the pair *together* — one without the other is a scope error —
so injecting both uniformly is simpler than a per-tool rule.

### 2. `rank` is two fields wearing one name

The wire shape `PageHit.rank` is overloaded per call, and conflating the two
readings corrupts both at once:

- On `memory_query`, it is a **relevance rank, lower is better** — FTS5 rank
  fused with entity/vector/graph streams and a bounded authority adjustment.
  Live-verified: a page matching `"stateless JWTs"` reported `rank:
  -0.018032786885245903`. That it is *negative* is why the adapter negates
  rather than reciprocates — `1/(1+rank)` is undefined for the ranks FTS5
  actually produces.
- On `memory_recent`, the store's query is `CAST(updated_at AS REAL) AS rank`
  under `ORDER BY updated_at DESC`. It is the page's **change time in
  microseconds since the Unix epoch**. Live-verified: a page written moments
  earlier reported `rank: 1790482892972451.0` — 2026-09-27 in microseconds.

The adapter keeps the two readings separate through a small `RankMeaning`
enum, and each path reports a different pairing:

- a **query** hit has `score: Some(-rank)` and takes `updated_at` from the
  fully-read page's frontmatter stamp;
- a **recent** hit has `score: None` and takes `updated_at` from the microsecond
  column, which is the very value the substrate orders by;
- a **written** page has neither, because the write path sends no rank.

This is the reason `Memory::updated_at` in `director-domain` is `Option`: the
field's source differs per path, and on a path where neither the column nor the
stamp is present it is genuinely absent rather than imputable.

### 3. Bodies are fetched, not snippeted

Search hits carry only an FTS5 snippet: a fragment around the matched terms,
marked up with `<mark>` tags. Orqyn's `Memory::body` is documented as
markdown, and a `<mark>`-tagged fragment is not markdown — it is not even the
page.

So the adapter fetches each hit's full page with `memory_read_page`. That is an
N+1 in round-trips, and it is the substrate's shape rather than a choice,
exactly as the handoff adapter fetches a full task record per id in
`list_tasks`. It also buys the only path to `tags`: hits do not carry them;
only a fully-read page's frontmatter does.

A hit whose page vanished between the search and the read is **skipped**, not
returned with its snippet as the body. The handoff adapter does the same for a
task whose fetch returns nothing. The alternative — silently reporting a
snippet as markdown — would violate the field's contract in exactly the way the
boundary exists to prevent.

### 4. Titles ride as a markdown H1

ai-memory derives a page's title from the first `# H1` in the body and asks
callers to prefer that over the `title` argument, which is a documented source
of JSON-escaping failures when the title contains quotes or colons (issue #67).

The adapter prepends `# {title}` to the body and omits `title` entirely. The
substrate then derives the title from the H1 — the same string — so the round
trip is exact even for `Why 2 + 2 = 4: a proof`. Read-back after write is what
makes this safe: whatever the substrate actually stored is what the caller sees,
so any mangling is visible rather than silent.

## What the adapter deliberately does not do

**It does not create the project up front.** ai-memory creates a project on the
first page written into it, and reads of a project that does not exist fail
*closed* — `lookup_existing` in the scope resolver returns an error, which the
server maps to `invalid_params`. That fail-closed behavior is an isolation
guard, not an inconvenience: it is what stops an ambiguous scope from resolving
to another project's memory.

Converting that error into an empty result would mask the exact misconfiguration
the guard exists to catch. So the adapter propagates it, and a caller that
queries before writing gets an error it can act on. The live suite asserts this
behavior explicitly.

**`MemoryQuery::tags` is ignored, not honored.** ai-memory's query has no tag
filter argument — only `workspace`/`project`/`scopes`. The trait documents tags
as a best-effort restriction, so silence is the sanctioned behavior; erroring
would make a query Orqyn can otherwise serve unusable.

**`kind` round-trips only when it names a tier.** Orqyn's `kind` is coarse;
ai-memory's `tier` is a retention class. The substrate stamps the tier into a
page's frontmatter on write, so a `kind` that is one of
`working`/`episodic`/`semantic`/`procedural` is the `kind` Orqyn reads back.

A kind that is *not* a tier — `decision`, say — cannot be stored: the substrate
rejects an unknown tier, and inventing a retention class for a kind that is not
one would misfile the page. So such a kind is dropped on write, and the tier
reported on read is then the substrate's default. The read-back returns what
the substrate actually stored, so the loss is visible to the caller rather than
hidden by echoing back what Orqyn was asked to save. A rewrite that omits a
tier resets it to the default — live-verified — which is a further reason
`save_memory` forwards the tier whenever Orqyn has one rather than relying
on a previous write's.

## One change outside the adapter

`Memory::updated_at` in `director-domain` changed from
`DateTime<Utc>` to `Option<DateTime<Utc>>`.

This is the same class of finding as Phase 2's closed `extra` channel: a field
the domain model assumed a substrate would provide, which one of them does not
provide on every path. The honest representation of "the substrate said
nothing" is `None`; back-filling it with `now()` would be inventing a fact, and
the in-memory provider's own module docs call that out as the failure mode the
trait exists to prevent.

The change is contained: the in-memory provider sorts on the option unchanged
(records with no change time sort last), and one domain test constructs the new
shape.

## What live verification caught

Phase 2's record on this was the argument for doing it again: two wire mirrors
that source-reading said were right and a live run proved wrong. Phase 3's live
probing found three things, two of which had been reasoned about from source and
gotten backwards.

**A latent Phase 2 bug: the shared transport could not parse either substrate.**
`stdio_mcp.rs` deserialized each content block from a field named `kind`. Both
substrates send the MCP spec's name, `type`. Every tool call — handoff and
ai-memory alike — was failing with `bad result envelope: missing field \`kind\``.
The handoff live suite had been green when it was written; the field was
renamed some time after, and nothing re-ran the suite, so the regression sat
latent through the rest of Phase 2 and would have shipped. Fixed with a
`#[serde(rename = "type", alias = "kind")]`, and both live suites now pass —
13 green live tests where an hour before there were zero.

**`kind` does round-trip after all.** Reading the substrate's source suggested
the tier a page was written with never reached the read path, so the adapter
was written to report `kind: None` and the doc above recorded that as a
limitation. The live `memory_read_page` reply shows the substrate *stamps the
tier into frontmatter*: `{"tier": "semantic", "type": "Note", "generated": {...}}`.
So the adapter reads it, and the limitation is smaller than first written — a
kind round-trips when it names a tier and is dropped otherwise.

**A change time is available on the query path.** A search hit carries no
timestamp, which made `updated_at` look query-path-empty. But every page's
frontmatter carries `generated.at`, an RFC3339 instant stamped per write, and
the adapter already fetches the full page for every hit — so query hits can
report a change time after all. Live-verified twice over: writing a page,
pausing, rewriting it, and reading back moved the stamp from `04:28:24Z` to
`04:28:28Z`, so it tracks the latest version rather than the page's creation.
The recent path keeps the microsecond column, which is the value the substrate
itself orders by.

The shape of the lesson is the same as Phase 2's and worth stating plainly:
**source reading tells you what a struct contains; only a live run tells you
what reaches the wire.** Three of this phase's findings were of that kind.

## Evidence

### Gates

All four are clean as of this writing:

```sh
cargo fmt --all -- --check                # clean
cargo clippy --all --all-targets -- -D warnings   # clean
cargo test --all                          # 209 tests, 0 failures
cargo build --all                         # clean
```

### Test counts and what each is evidence of

| Tests | About |
|---|---|
| 128 `director-domain` unit | Entity invariants from Phase 1, unchanged; one test updated for the optional `updated_at`. |
| 77 `director-adapters` unit | The adapters crate, of which 19 are new: 14 driving `AiMemoryAdapter` through a fake `AiMemoryWire`, 5 parsing wire mirrors. |
| 3 `tests/boundary.rs` | The substrate-coupling rule: nothing outside `director-adapters` may name a substrate. |
| 8 `tests/aimemory_live.rs`, `#[ignore]` | **Live** runs against a real `ai-memory` server (v2.4.0). |
| 5 `tests/handoff_live.rs`, `#[ignore]` | Phase 2's live suite, green again after the shared-transport fix. |
| 1 doc-test | The id generator example. |

The 14 adapter unit tests drive a fake that mirrors what the substrate *should*
do: the rank split, the scoping, the H1 titles, the tier mapping, the limit
clamp, and the skip-not-snippet rule. They are evidence about the adapter's
logic and deliberately *not* evidence about the wire — a fake cannot disagree
with the mirror it was written from. That is the live suite's job.

The eight live tests are ignored by default because they need the built server
binary and write a wiki and index onto the filesystem:

```sh
AI_MEMORY_BINARY=/path/to/ai-memory(.exe) \
  cargo test -p director-adapters --test aimemory_live -- --ignored --nocapture
```

They cover the query round trip, an absent query, the punctuation title, both
sources of `updated_at`, the tag round trip, the tier round trip, and the
fail-closed read of a project that was never written.

## Definition of done — status

- [x] `AiMemoryAdapter` implements `MemoryProvider` over a live ai-memory.
- [x] The overloaded `rank` field is handled correctly on both read paths.
- [x] `Memory::updated_at` is optional, and the reason is documented at the
      field.
- [x] Wire mirrors verified against the live server.
- [x] The shared transport's content-block bug found and fixed, and Phase 2's
      live suite re-verified green.
- [x] All four gates clean.

## What Phase 3 deliberately does not do

- **No other provider trait.** `AiMemoryAdapter` implements `MemoryProvider`
  only. ai-memory's handoff rows are session-scoped notes, not Orqyn's
  claim-once transfer; its session records are the handoff adapter's half of the
  boundary; observing git is the `git` module; running commands is
  `LocalExecutor`.
- **No project registry wiring.** The workspace and project names are
  configuration. Which ai-memory workspace and project correspond to a Orqyn
  `Project` is a mapping Phase 5 owns, when Orqyn persists its own entities.
- **No consolidation or decay tuning.** The substrate's compaction, lint, and
  auto-improvement loops are its to run. Orqyn reads and writes pages; it
  does not manage the wiki.
- **No multi-project search.** `scopes` and `global=true` are deliberately not
  used. Orqyn's reads are pinned to one project by design (see §1), and a
  cross-project query is a different feature with different failure modes.
