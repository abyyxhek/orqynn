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
    // names one is the kind Director reads back. Live-verified because the
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
