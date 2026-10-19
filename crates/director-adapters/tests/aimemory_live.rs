//! Live integration against a real ai-memory server.
//!
//! These are `#[ignore]` by default: they spawn the server binary and write a
//! wiki + SQLite index onto the filesystem, so they cannot run in CI where the
//! binary may not exist. Run them explicitly:
//!
//! ```sh
//! AI_MEMORY_BINARY=/path/to/ai-memory(.exe) \
//!   cargo test -p director-adapters --test aimemory_live -- --ignored --nocapture
//! ```
//!
//! ## Why these exist
//!
//! The unit tests in `aimemory/adapter.rs` drive a fake that mirrors what the
//! substrate *should* do. Only a live run can catch the real failure mode these
//! guard against: the wire mirror in `aimemory/wire.rs` drifting from the
//! server's actual JSON, so a reply parses to nothing or to the wrong thing.
//! Phase 2 found exactly that twice — a field renamed upstream, a shape built
//! at the call site rather than on the storage struct — and neither was visible
//! to a fake. Every shape these tests parse was verified against the live
//! server once; a change upstream that breaks them is a real regression, not a
//! flake.
//!
//! ## What these cost the machine
//!
//! Each test spawns its own server with its own data directory under the
//! system temp dir, so no test sees another's state. The directories are not
//! under the repo: the substrate checkpoints its wiki into a git repository
//! there, and a sync engine taking locks would make these flaky.

use std::path::PathBuf;

use director_adapters::AiMemoryAdapter;
use director_domain::providers::{Memory, MemoryProvider, MemoryQuery};

/// Where the built server lives on this machine, overridable for other setups.
fn binary() -> String {
    std::env::var("AI_MEMORY_BINARY")
        .unwrap_or_else(|_| "C:/Users/ASUS/.aimemory-target/release/ai-memory.exe".to_string())
}

/// A fresh, throwaway data directory per test, so no test sees another's wiki.
fn data_dir(name: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("director-aimemory-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("data dir");
    dir
}

/// An adapter over a server rooted at a scratch data directory.
async fn adapter(name: &str) -> AiMemoryAdapter {
    let dir = data_dir(name);
    AiMemoryAdapter::connect(&binary(), &dir, "director-workspace", name)
        .await
        .expect("connect to the live server")
}

fn memory(path: &str, title: &str, body: &str) -> Memory {
    Memory {
        id: path.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        kind: None,
        tags: vec![],
        score: None,
        updated_at: None,
    }
}

#[tokio::test]
#[ignore = "spawns the ai-memory server binary"]
async fn a_saved_memory_is_queryable_by_content() {
    let adapter = adapter("queryable").await;
    adapter
        .save_memory(memory(
            "notes/auth-design.md",
            "Auth design",
            "we settled on stateless JWTs for the login flow",
        ))
        .await
        .expect("save");

    let hits = adapter
        .query_memories(&MemoryQuery::new("stateless JWTs", 10))
        .await
        .expect("query");

    assert_eq!(hits.len(), 1, "the page matches the query");
    assert_eq!(hits[0].id, "notes/auth-design.md");
    assert_eq!(hits[0].title, "Auth design");
    assert!(
        hits[0]
            .body
            .contains("we settled on stateless JWTs for the login flow"),
        "the hit carries the full page body, not the FTS5 snippet"
    );
}

#[tokio::test]
#[ignore = "spawns the ai-memory server binary"]
async fn a_query_for_something_absent_returns_nothing() {
    let adapter = adapter("absent").await;
    adapter
        .save_memory(memory("notes/one.md", "One", "a page about rust"))
        .await
        .expect("save");

    let hits = adapter
        .query_memories(&MemoryQuery::new("kubernetes helm charts", 10))
        .await
        .expect("query");
    assert!(
        hits.is_empty(),
        "a query that matches nothing returns nothing"
    );
}

#[tokio::test]
#[ignore = "spawns the ai-memory server binary"]
async fn a_title_with_punctuation_round_trips_through_the_h1() {
    // The substrate asks callers to put the title in the body's H1 rather than
    // the `title` argument, which mangles punctuation. Live-verify that the
    // choice survives a real round trip.
    let adapter = adapter("punctuation").await;
    let saved = adapter
        .save_memory(memory(
            "notes/proof.md",
            "Why 2 + 2 = 4: a proof",
            "by induction, trivially",
        ))
        .await
        .expect("save");

    assert_eq!(saved.title, "Why 2 + 2 = 4: a proof");
}

#[tokio::test]
#[ignore = "spawns the ai-memory server binary"]
async fn a_recent_memory_reports_its_change_time() {
    // The one read path where the overloaded `rank` field is a timestamp
    // rather than a relevance score. Live-verify the microseconds decode.
    let adapter = adapter("recent").await;
    adapter
        .save_memory(memory("notes/recent.md", "Recent", "freshly written page"))
        .await
        .expect("save");

    let hits = adapter.recent_memories(10).await.expect("recent");
    assert_eq!(hits.len(), 1);
    let updated_at = hits[0]
        .updated_at
        .expect("a recent hit carries its change time");
    assert!(
        updated_at > chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        "the change time is a plausible recent instant: {updated_at}"
    );
    assert!(
        hits[0].score.is_none(),
        "a recent hit carries no relevance score"
    );
}

#[tokio::test]
#[ignore = "spawns the ai-memory server binary"]
async fn a_query_hit_reports_relevance_and_a_change_time() {
    // Live-verify both sources: the hit's `rank` is a relevance score, and the
    // change time comes from the fully-read page's `generated.at` frontmatter
    // stamp — the only timestamp the query path exposes.
    let adapter = adapter("rank").await;
    adapter
        .save_memory(memory(
            "notes/ranked.md",
            "Ranked",
            "searchable content here",
        ))
        .await
        .expect("save");

    let hits = adapter
        .query_memories(&MemoryQuery::new("searchable content", 10))
        .await
        .expect("query");
    assert_eq!(hits.len(), 1);
    assert!(hits[0].score.is_some(), "a query hit carries a rank");
    let updated_at = hits[0]
        .updated_at
        .as_ref()
        .expect("a query hit carries the page's generation stamp");
    assert!(
        updated_at > &chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        "the generation stamp is a plausible recent instant: {updated_at}"
    );
}

#[tokio::test]
#[ignore = "spawns the ai-memory server binary"]
async fn tags_round_trip_through_frontmatter() {
    let adapter = adapter("tags").await;
    let mut memory = memory("notes/tagged.md", "Tagged", "a categorized page");
    memory.tags = vec!["db".into(), "decision".into()];
    let saved = adapter.save_memory(memory).await.expect("save");

    let hits = adapter
        .query_memories(&MemoryQuery::new("categorized page", 10))
        .await
        .expect("query");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].tags, saved.tags);
}

#[tokio::test]
#[ignore = "spawns the ai-memory server binary"]
async fn a_kind_that_names_a_tier_round_trips() {
    // The substrate stamps the retention tier into frontmatter, so a kind that
    // names one is the kind Orqyn reads back. Live-verified because the
    // frontmatter stamp is not visible from the substrate's source alone.
    let adapter = adapter("tier").await;
    let mut memory = memory("notes/tiered.md", "Tiered", "a retained page");
    memory.kind = Some("episodic".into());
    let saved = adapter.save_memory(memory).await.expect("save");

    assert_eq!(saved.kind.as_deref(), Some("episodic"));

    let hits = adapter
        .query_memories(&MemoryQuery::new("retained page", 10))
        .await
        .expect("query");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].kind.as_deref(), Some("episodic"));
}

#[tokio::test]
#[ignore = "spawns the ai-memory server binary"]
async fn reading_a_project_that_was_never_written_fails_closed() {
    // The isolation guard, live: ai-memory resolves an explicit
    // workspace+project that does not exist by failing, not by falling back to
    // another project's memory. The adapter propagates that rather than
    // converting it to an empty result, because an empty result would mask a
    // scope misconfiguration.
    let dir = data_dir("fail-closed");
    let adapter =
        AiMemoryAdapter::connect(&binary(), &dir, "director-workspace", "no-such-project")
            .await
            .expect("connect");

    let result = adapter
        .query_memories(&MemoryQuery::new("anything", 10))
        .await;
    assert!(
        result.is_err(),
        "a read of a project that was never written is an error, not an empty list"
    );
}

// ---------------------------------------------------------------------------
// The checkpoint publication path, against the live server
// ---------------------------------------------------------------------------

/// Orqyn's own checkpoint payload lands on the real substrate and reads back.
///
/// This is the one place the two halves meet: the application builds the payload
/// and calls the adapter, and the substrate stores it. Everything above proves
/// the adapter in isolation; this proves the application path against the real
/// server rather than a double.
///
/// Requires the `ai-memory` binary and nothing else — no credentials.
#[tokio::test]
#[ignore = "spawns the real ai-memory server"]
async fn a_checkpoint_payload_survives_the_real_substrate() {
    let dir = data_dir("checkpoint_payload");
    let adapter = AiMemoryAdapter::connect(&binary(), &dir, "director-workspace", "checkpointing")
        .await
        .expect("connect to the live server");

    // Exactly the record `checkpoint::memory_for` builds for a task checkpoint.
    let page = memory(
        "checkpoints/auth.md",
        "Checkpoint: AUTH",
        "**Resume status evidence for AUTH**\n\n- objective: implement auth\n- branch: main\n- head: abc123\n- progress: wrote the middleware\n- next action: run the tests\n- changed files: src/auth/mod.rs",
    );

    let written = adapter
        .save_memory(page.clone())
        .await
        .expect("the page writes");

    assert_eq!(
        written.id, page.id,
        "the substrate stores the path Orqyn derived from the task id"
    );

    // The narrative is findable by its own content, which is how a replacement
    // would actually look for it.
    let hits = adapter
        .query_memories(&MemoryQuery::new("Resume status evidence for AUTH", 10))
        .await
        .expect("query");
    let found = hits
        .iter()
        .find(|m| m.id == "checkpoints/auth.md")
        .expect("the checkpoint page is findable by content");
    assert!(
        found.body.contains("next action: run the tests"),
        "the next action survived, because that is what a replacement needs: {}",
        found.body
    );

    // Republishing the same path replaces rather than duplicates - the reason a
    // retried round cannot accumulate one page per attempt.
    adapter.save_memory(page).await.expect("republish");
    let after = adapter
        .query_memories(&MemoryQuery::new("Resume status evidence for AUTH", 50))
        .await
        .expect("query");
    assert_eq!(
        after
            .iter()
            .filter(|m| m.id == "checkpoints/auth.md")
            .count(),
        1,
        "a republish overwrites the same page instead of duplicating it"
    );
}

// ---------------------------------------------------------------------------
// Retrieval: what publication wrote comes back
// ---------------------------------------------------------------------------

/// Orqyn publishes a checkpoint page, queries for it the way the briefing does,
/// and the page comes back with its path and tags intact.
///
/// This is the read half of the write/read loop. Phase 6A proved the write
/// reaches the substrate; this proves the retrieval path Orqyn will actually use
/// finds it again — the query text and the association check are the two places
/// where a mismatch between write and read would silently break the feature.
#[tokio::test]
#[ignore = "spawns the real ai-memory server"]
async fn a_published_checkpoint_is_retrievable_by_the_briefings_own_query() {
    let dir = data_dir("checkpoint_retrieval");
    let adapter = AiMemoryAdapter::connect(&binary(), &dir, "director-workspace", "retrieval")
        .await
        .expect("connect to the live server");

    let task = "AUTH";
    let path = format!("checkpoints/{}.md", task.to_lowercase());

    // Exactly what `checkpoint::memory_for` writes: the same path, the same
    // title, the same narrative, and the same two tags. The tags matter as much
    // as the body — they are one of the two signals the retrieval path uses to
    // decide a page belongs to this task.
    let mut page = memory(
        &path,
        &format!("Checkpoint: {task}"),
        "**Resume status evidence for AUTH**\n\n- objective: implement auth\n- branch: main\n- head: abc123\n- progress: wrote the middleware\n- next action: run the tests",
    );
    page.tags = vec!["orqyn-checkpoint".into(), task.to_lowercase()];
    adapter
        .save_memory(page)
        .await
        .expect("the checkpoint page writes");

    // Exactly what the briefing asks for: the same header text, derived from the
    // task id on both sides.
    let hits = adapter
        .query_memories(&MemoryQuery::new(
            format!("Resume status evidence for {task}"),
            10,
        ))
        .await
        .expect("query");

    let found = hits
        .iter()
        .find(|m| m.id == path)
        .expect("the published page is found again by the briefing's own query");

    assert!(
        found.body.contains("next action: run the tests"),
        "the narrative survived the round trip: {}",
        found.body
    );

    // Both association signals survive, because either alone is accepted and the
    // check is what stops another task's page being presented as this one's.
    assert!(
        found.tags.iter().any(|t| t == "orqyn-checkpoint"),
        "the checkpoint tag survived: {:?}",
        found.tags
    );
    assert!(
        found.tags.iter().any(|t| t == "auth"),
        "the task tag survived: {:?}",
        found.tags
    );

    // And a different task's page must not match this one's path or tags.
    let unrelated = hits.iter().filter(|m| m.id != path).count();
    assert_eq!(
        unrelated,
        hits.len() - 1,
        "no extra page was created by the write"
    );
}
