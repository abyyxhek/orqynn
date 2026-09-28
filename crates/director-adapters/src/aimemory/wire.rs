//! Mirrors of the ai-memory wire shapes Orqyn parses.
//!
//! Every struct here is a hand-written mirror of a JSON reply the live
//! `ai-memory serve` server actually emits, verified against a running binary
//! rather than only against the substrate's source. Source reading is not
//! enough: the reply a tool *returns* is built at the call site and can drop,
//! rename, or overload fields the storage struct carries, and a mirror built
//! from the wrong one parses nothing on a live run while every fake-based unit
//! test still passes. See `docs/PHASE3-MEMORY.md` for the specific cases this
//! caught.
//!
//! Deserialization is deliberately permissive about *extra* fields — serde
//! ignores them by default — and strict about the ones Orqyn reads. That is
//! the right combination: an upstream field Orqyn does not care about must
//! not break the adapter, while a field Orqyn does care about going missing
//! must surface as a [`crate::aimemory::adapter::AiMemoryAdapterError::Malformed`]
//! rather than silently as `None`.
//!
//! ## The `rank` field is overloaded
//!
//! [`PageHit::rank`] means two different things depending on which tool
//! produced it, and getting that wrong corrupts both readings at once:
//!
//! - On `memory_query` it is a **relevance rank, lower = better** (FTS5 rank
//!   fused with entity/vector/graph streams and a bounded authority
//!   adjustment).
//! - On `memory_recent` the store puts `CAST(updated_at AS REAL)` into the same
//!   slot — it is the page's **change time in microseconds since the Unix
//!   epoch**, and the `ORDER BY updated_at DESC` around it is what makes the
//!   listing recency-ordered.
//!
//! So a `memory_recent` hit carries a change time and no relevance score, and
//! a `memory_query` hit carries a relevance score and no change time. The
//! adapter keeps the two readings separate rather than trying to unify them,
//! which is also why [`director_domain::providers::Memory`] has an optional
//! `updated_at` and an optional `score`.

/// One hit in a `memory_query` or `memory_recent` reply.
///
/// Mirrors `ai_memory_store::PageHit` as serialized by the MCP server —
/// specifically the fields Orqyn reads. The store struct also carries an
/// `id` (a per-version [`PageId`]), plus `superseded` and `pinned` flags that
/// are omitted from JSON when false; none of them are stable or meaningful
/// enough to become Orqyn's identity, so they are not mirrored.
///
/// [`PageId`]: ai_memory_store::PageId
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct PageHit {
    /// Relative wiki path, e.g. `notes/auth-design.md`. The only stable,
    /// caller-visible identity a page has — the version id changes on every
    /// edit — so this is what the adapter uses as Orqyn's `Memory::id`.
    pub path: String,
    /// Page title.
    pub title: String,
    /// FTS5 snippet of the body around the matched terms, HTML-marked with
    /// `<mark>` tags. Deliberately *not* used as `Memory::body`: a snippet is
    /// a fragment with markup in it, not the page's markdown. The adapter
    /// fetches the full page instead.
    pub snippet: String,
    /// Overloaded per call — see the module docs. Relevance rank (lower is
    /// better) from `memory_query`; change time in microseconds from
    /// `memory_recent`.
    pub rank: f64,
}

/// The `memory_query` reply.
///
/// The server's response also carries `answer`, `raw_hits`, `global_hits`,
/// `global_scope_hits`, and `streams_active` — none populated on the calls
/// Orqyn makes (no `answer=true`, no `global`, no `scopes`, no `explain`),
/// and all omitted from JSON when empty. Mirroring only `hits` keeps the mirror
/// honest about what Orqyn actually reads.
#[derive(Debug, serde::Deserialize)]
pub struct MemoryQueryResponse {
    /// Ranked hits, best first.
    pub hits: Vec<PageHit>,
}

/// The `memory_recent` reply.
///
/// `global_hits` is populated only when a repo opts into `[recall]
/// default_global` *and* the call is unscoped. Orqyn always sends an
/// explicit `workspace` + `project`, which makes that branch unreachable, so
/// the field is not mirrored: if it ever did fire, `hits` would be empty and
/// the adapter would report no recent memory rather than silently reporting
/// another project's.
#[derive(Debug, serde::Deserialize)]
pub struct MemoryRecentResponse {
    /// Recent pages, newest first.
    pub hits: Vec<PageHit>,
}

/// The `memory_read_page` reply — one page's full markdown.
#[derive(Debug, serde::Deserialize)]
pub struct ReadPageReply {
    /// Relative wiki path.
    pub path: String,
    /// Title: the frontmatter `title`, else the first `# H1` in the body, else
    /// the path stem. The substrate derives it rather than trusting stored
    /// frontmatter.
    pub title: String,
    /// The full body as markdown, without frontmatter.
    pub body: String,
    /// Parsed frontmatter, or JSON null when the page has none. This is where
    /// `tags` live — hits do not carry them.
    #[serde(default)]
    pub frontmatter: serde_json::Value,
}

/// The `memory_write_page` reply.
///
/// `checkpoint` is a git-commit SHA when the wiki layer checkpoints; Orqyn
/// does not consume it, so it is not mirrored.
#[derive(Debug, serde::Deserialize)]
pub struct WritePageReply {
    /// The substrate's own page id. Opaque to Orqyn; the path is the
    /// identity Orqyn keeps.
    #[allow(dead_code)]
    pub page_id: String,
    /// The path the substrate stored the page at, which Orqyn reads back
    /// rather than trusting the path it asked for.
    pub path: String,
}

/// The four memory tiers ai-memory recognises, for `memory_write_page`'s
/// `tier` argument.
///
/// Kept as a list of `&'static str` rather than an enum because the substrate
/// parses them from a string and may add a fifth; a value Orqyn does not
/// know should be passed through or dropped, not rejected at compile time.
pub(crate) const KNOWN_TIERS: [&str; 4] = ["working", "episodic", "semantic", "procedural"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_hit_parses() {
        // Shaped like a real `memory_query` hit: the fields Orqyn reads,
        // plus one it does not (`score_details`) that must not break parsing.
        let json = r#"{
            "id": "01J6Z4KQ4N",
            "path": "notes/auth-design.md",
            "title": "Auth design",
            "snippet": "we chose <mark>stateless</mark> JWTs",
            "rank": -3.2,
            "score_details": {"rrf": 0.81}
        }"#;
        let hit: PageHit = serde_json::from_str(json).expect("parse");
        assert_eq!(hit.path, "notes/auth-design.md");
        assert_eq!(hit.title, "Auth design");
        assert_eq!(hit.snippet, "we chose <mark>stateless</mark> JWTs");
    }

    #[test]
    fn a_query_hit_missing_a_field_director_reads_is_an_error() {
        // The canary: if the substrate renames `title`, this fails and the
        // adapter reports Malformed instead of quietly returning empty.
        let json = r#"{"path": "notes/x.md", "snippet": "s", "rank": 1.0}"#;
        assert!(serde_json::from_str::<PageHit>(json).is_err());
    }

    #[test]
    fn a_recent_reply_parses_without_the_global_hits_field() {
        let json = r#"{"hits": [{"path": "a.md", "title": "A", "snippet": "s", "rank": 1759000000000000}]}"#;
        let reply: MemoryRecentResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(reply.hits.len(), 1);
    }

    #[test]
    fn a_write_reply_parses_without_the_checkpoint_field() {
        let json = r#"{"page_id": "01J6Z4KQ", "path": "notes/a.md", "checkpoint": "abc123"}"#;
        let reply: WritePageReply = serde_json::from_str(json).expect("parse");
        assert_eq!(reply.path, "notes/a.md");
    }

    #[test]
    fn a_read_reply_parses_a_null_frontmatter() {
        // A page with no frontmatter is serialized as JSON null, not omitted.
        // Built with the `json!` macro rather than a raw string: the body
        // contains `"# A\n\nbody"`, whose `"#` terminates an `r#"..."#` literal.
        let json = serde_json::json!({
            "path": "a.md",
            "title": "A",
            "body": "# A\n\nbody",
            "frontmatter": null,
        })
        .to_string();
        let reply: ReadPageReply = serde_json::from_str(&json).expect("parse");
        assert!(reply.frontmatter.is_null());
        assert_eq!(reply.body, "# A\n\nbody");
    }

    #[test]
    fn tags_are_read_from_frontmatter() {
        let json = r#"{
            "path": "a.md",
            "title": "A",
            "body": "body",
            "frontmatter": {"title": "A", "tags": ["auth", "decision"]}
        }"#;
        let reply: ReadPageReply = serde_json::from_str(json).expect("parse");
        let tags = reply.frontmatter.get("tags").and_then(|v| v.as_array());
        assert_eq!(
            tags.map(|t| t.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()),
            Some(vec!["auth", "decision"])
        );
    }
}
