//! [`AiMemoryAdapter`] — Orqyn's [`MemoryProvider`] over a live ai-memory.
//!
//! This is Phase 3: the struct that makes long-term memory real by composing
//! the two layers underneath it — [`transport`] to reach the substrate, and
//! [`wire`] to parse what comes back.
//!
//! ## What the adapter adds on top of the two layers
//!
//! The wire mirror is stateless and the transport is a pipe; the mapping
//! between ai-memory's model and Orqyn's is what lives here. Four things,
//! each a consequence of a real difference between the two models:
//!
//! 1. **Scoping.** Orqyn is a static MCP client — no lifecycle-hook session
//!    id is bridged onto its requests — so the substrate's session-based scope
//!    routing is unavailable. The adapter sends `workspace` and `project` on
//!    every call, which pins each read to exactly one project and never falls
//!    back to another project's memory.
//! 2. **The overloaded `rank` field.** A [`PageHit`](crate::aimemory::wire::PageHit)'s
//!    `rank` is a relevance score from `memory_query` and a *change time in
//!    microseconds* from `memory_recent`. The adapter keeps the two readings
//!    separate rather than unifying them: a query hit's `score` is its negated
//!    rank and its `updated_at` comes from the page's frontmatter stamp, while
//!    a recent hit reports no score at all and takes `updated_at` from the
//!    microsecond column — the value the substrate itself orders by.
//! 3. **Bodies are fetched, not snippeted.** Search hits carry only an
//!    HTML-marked FTS5 fragment. [`Memory::body`] is documented as markdown, so
//!    the adapter fetches each hit's full page — an N+1 in round-trips, the
//!    substrate's shape rather than a choice, exactly as the handoff adapter
//!    fetches full task records.
//! 4. **Titles ride as a markdown H1.** The substrate derives a page's title
//!    from the first `# H1` in the body and asks callers to prefer that over
//!    the `title` argument, which is a known source of JSON-escaping failures.
//!    The adapter prepends `# {title}` to the body and omits `title`, so what
//!    comes back is byte-identical to what was asked for.
//!
//! ## What the adapter deliberately does not do
//!
//! It does not create the project up front. ai-memory creates a project on the
//! first page written into it, and reads of a project that does not exist fail
//! *closed* — an isolation guard, not an inconvenience. Converting that failure
//! into an empty result would mask a scope misconfiguration the same guard
//! exists to catch, so the adapter propagates it and lets the caller decide.
//!
//! [`MemoryQuery::tags`] is ignored rather than erroring: ai-memory's query has
//! no tag filter, and the trait documents tags as a best-effort restriction.
//!
//! ## The test seam
//!
//! Every method goes through [`AiMemoryWire`], a one-method trait that the real
//! [`MemoryTransport`] satisfies and a fake can satisfy too. The adapter's
//! logic — the rank split, the scoping, the H1 titles — is therefore
//! unit-testable with no child process, and the tests here are evidence about
//! the adapter rather than about a spawn succeeding. The shapes they parse were
//! verified against a live server once; the `#[ignore]` live tests keep them
//! honest.
//!
//! [`Memory`]: director_domain::providers::Memory
//! [`Memory::body`]: director_domain::providers::Memory::body
//! [`MemoryQuery::tags`]: director_domain::providers::MemoryQuery::tags
//! [`MemoryProvider`]: director_domain::providers::MemoryProvider

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use director_domain::providers::{Memory, MemoryProvider, MemoryQuery, Provider};

use crate::aimemory::transport::MemoryTransport;
use crate::aimemory::wire::{
    MemoryQueryResponse, MemoryRecentResponse, PageHit, ReadPageReply, WritePageReply, KNOWN_TIERS,
};

/// Which of its two meanings a [`PageHit`]'s `rank` carries, per the tool that
/// produced it. See the [`wire`] module docs for why the split is load-bearing.
enum RankMeaning {
    /// From `memory_query`: a relevance rank, lower is better.
    Relevance(f64),
    /// From `memory_recent`: the page's change time, in microseconds since the
    /// Unix epoch.
    Recent(f64),
    /// Neither applies — the record came from the write path, which reports no
    /// rank at all.
    None,
}

/// Errors the ai-memory adapter can produce.
///
/// A type of its own rather than reusing `ProviderError`, for the same reason
/// the handoff adapter's is: the failures here are substrate-specific (a tool
/// rejected the call, a reply would not parse) and folding them into a generic
/// vocabulary would lose the message that explains how to fix the request.
#[derive(Debug, thiserror::Error)]
pub enum AiMemoryAdapterError {
    /// The substrate could not be reached or replied with malformed JSON-RPC.
    #[error("ai-memory transport failed: {0}")]
    Transport(#[from] crate::aimemory::transport::TransportError),
    /// The substrate ran the tool and it reported failure (`isError`).
    #[error("ai-memory tool '{tool}' failed: {message}")]
    Tool {
        /// The tool that was called.
        tool: &'static str,
        /// The message the substrate returned.
        message: String,
    },
    /// A reply could not be parsed into the shape Orqyn expects — a sign a
    /// mirror in [`wire`] has drifted from the live server.
    #[error("could not parse ai-memory's reply to '{tool}': {message}")]
    Malformed {
        /// The tool that was called.
        tool: &'static str,
        /// Why parsing failed.
        message: String,
    },
}

/// A connection that can call ai-memory tools.
///
/// One method, so that [`MemoryTransport`] and a test fake are interchangeable.
#[async_trait]
pub trait AiMemoryWire: Send {
    /// Call `name` with `arguments`, returning the tool's text content.
    async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<String, AiMemoryAdapterError>;
}

#[async_trait]
impl AiMemoryWire for MemoryTransport {
    async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<String, AiMemoryAdapterError> {
        // Fully-qualified so the inherent method resolves over the trait one.
        MemoryTransport::call_tool(self, name, arguments)
            .await
            .map_err(AiMemoryAdapterError::from)
    }
}

/// Orqyn's adapter for the ai-memory substrate.
///
/// Generic over the wire connection so tests can drive it with a fake; the
/// default is the real stdio transport.
pub struct AiMemoryAdapter<T = MemoryTransport> {
    /// The substrate connection. Mutex'd because a stdin/stdout pair is mutated
    /// by every call, and the provider traits are `&self`.
    transport: Mutex<T>,
    /// Workspace name, sent with every call. See the module docs on scoping.
    workspace: String,
    /// Project name, sent with every call.
    project: String,
}

impl<T: AiMemoryWire> AiMemoryAdapter<T> {
    /// Wrap an already-connected transport.
    ///
    /// Does no I/O: it neither creates the project nor checks it exists, so a
    /// test can construct the adapter and arrange state through the fake first.
    /// The first `save_memory` creates the project; reads of a missing project
    /// fail closed, and the adapter propagates that rather than masking it.
    pub fn new(transport: T, workspace: impl Into<String>, project: impl Into<String>) -> Self {
        AiMemoryAdapter {
            transport: Mutex::new(transport),
            workspace: workspace.into(),
            project: project.into(),
        }
    }

    /// Call an ai-memory tool directly, bypassing the mapping.
    ///
    /// Not part of any provider trait and never will be: it exists so a live
    /// integration test can reproduce substrate-level state Orqyn itself
    /// would never make — a page written out of band, or a scope mismatch —
    /// and then observe how the adapter responds. Orqyn's own code goes
    /// through the trait methods.
    pub async fn raw_call(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<String, AiMemoryAdapterError> {
        let mut transport = self.transport.lock().await;
        transport.call_tool(name, arguments).await
    }

    /// Call a tool and return its raw text reply.
    ///
    /// Every project-scoped tool takes `workspace` and `project` *together*;
    /// sending one without the other is a scope error on this substrate, and
    /// sending neither falls back to session routing Orqyn cannot use.
    /// Injecting both uniformly is simpler than a per-tool rule and is the only
    /// scoping that is correct for a static client.
    async fn call(
        &self,
        tool: &'static str,
        mut arguments: Value,
    ) -> Result<String, AiMemoryAdapterError> {
        if let Some(obj) = arguments.as_object_mut() {
            obj.entry("workspace")
                .or_insert(Value::String(self.workspace.clone()));
            obj.entry("project")
                .or_insert(Value::String(self.project.clone()));
        }
        let mut transport = self.transport.lock().await;
        transport
            .call_tool(tool, arguments)
            .await
            .map_err(|error| match error {
                // A tool-level failure keeps the tool name so the caller knows
                // which call to fix; everything else propagates as-is.
                AiMemoryAdapterError::Tool { message, .. } => {
                    AiMemoryAdapterError::Tool { tool, message }
                }
                other => other,
            })
    }

    /// Call a tool and parse its text reply as JSON.
    async fn call_json<R: DeserializeOwned>(
        &self,
        tool: &'static str,
        arguments: Value,
    ) -> Result<R, AiMemoryAdapterError> {
        let text = self.call(tool, arguments).await?;
        serde_json::from_str::<R>(&text).map_err(|message| AiMemoryAdapterError::Malformed {
            tool,
            message: format!(
                "{message}: {}",
                // The reply that failed to parse is the best diagnostic the
                // substrate gives us; truncate so a runaway reply cannot flood.
                text.chars().take(512).collect::<String>()
            ),
        })
    }

    /// Read one page's full markdown by path, or `None` if the substrate has no
    /// such page in this scope.
    async fn fetch_page(&self, path: &str) -> Result<Option<ReadPageReply>, AiMemoryAdapterError> {
        match self
            .call_json::<ReadPageReply>(
                "memory_read_page",
                json!({ "path": path }),
            )
            .await
        {
            Ok(page) => Ok(Some(page)),
            Err(AiMemoryAdapterError::Tool { message, .. })
                // The substrate has no not-found reply code; it sends an
                // internal_error whose text names the missing page. Matching on
                // it is a string contract with the substrate, recorded here so
                // the fragility is visible rather than buried.
                if message.contains("not found") || message.contains("no pages found") =>
            {
                Ok(None)
            }
            Err(other) => Err(other),
        }
    }

    /// Turn a search hit into a Orqyn memory, fetching its full page.
    ///
    /// Hits carry only a snippet; the body and the tags are not on the wire
    /// until the page is read. A hit whose page vanished between the search and
    /// the read is skipped rather than reported as a snippet-bodied memory —
    /// `Memory::body` is markdown, and a `<mark>`-tagged fragment is not.
    async fn materialize(
        &self,
        hit: PageHit,
        rank: RankMeaning,
    ) -> Result<Option<Memory>, AiMemoryAdapterError> {
        let Some(page) = self.fetch_page(&hit.path).await? else {
            return Ok(None);
        };
        Ok(Some(self.page_to_memory(page, rank)))
    }

    /// Assemble a Orqyn memory from a fully-read page plus the meaning of
    /// the `rank` the hit that led to it carried.
    fn page_to_memory(&self, page: ReadPageReply, rank: RankMeaning) -> Memory {
        Memory {
            // The path is the page's only stable identity: the version id the
            // substrate reports changes on every edit, so it cannot be the id
            // Orqyn keeps.
            id: page.path,
            title: page.title,
            body: page.body,
            // The substrate stamps the retention tier into frontmatter, so a
            // kind that names a tier is the kind Orqyn reads back. A kind
            // that does not is dropped on write, and the tier reported here is
            // then the substrate's default — not the kind Orqyn was asked to
            // store.
            kind: frontmatter_tier(&page.frontmatter),
            tags: frontmatter_tags(&page.frontmatter),
            score: match rank {
                // Orqyn's score is higher = more relevant; the substrate's
                // rank is lower = better. Negating preserves both the order and
                // the relative distances, which a reciprocal would not.
                RankMeaning::Relevance(rank) => Some(-rank),
                RankMeaning::Recent(_) | RankMeaning::None => None,
            },
            updated_at: match rank {
                // The recent path's `rank` is the store's own `updated_at`
                // column in microseconds — the very value the substrate orders
                // by — so it outranks the frontmatter stamp.
                RankMeaning::Recent(micros) => chrono::DateTime::from_timestamp_micros(
                    // The column is integer microseconds; the REAL cast and the
                    // f64 round trip are exact for every value in range, and one
                    // outside it falls back to the frontmatter stamp below.
                    micros as i64,
                )
                .or_else(|| generated_at(&page.frontmatter)),
                // Every other path has no timestamp on the wire; the substrate
                // stamps one into frontmatter at write time instead.
                _ => generated_at(&page.frontmatter),
            },
        }
    }
}

/// The tags on a page, read from its frontmatter.
///
/// Hits do not carry tags; only a fully-read page does. The substrate stores
/// the array under `tags` exactly as it was written.
fn frontmatter_tags(frontmatter: &Value) -> Vec<String> {
    frontmatter
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(String::from)
        .collect()
}

/// The retention tier a page was written with, read from its frontmatter.
fn frontmatter_tier(frontmatter: &Value) -> Option<String> {
    frontmatter
        .get("tier")
        .and_then(Value::as_str)
        .map(String::from)
}

/// When the substrate generated this page version, read from the frontmatter
/// stamp it writes on every page: `generated.at`, an RFC3339 instant.
///
/// This is the only change time available on the query path — a search hit
/// carries no timestamp of its own. It is stamped per write, so it tracks the
/// latest version rather than the page's creation.
fn generated_at(frontmatter: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    let instant = frontmatter.get("generated")?.get("at")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(instant)
        .ok()
        .map(|time| time.with_timezone(&chrono::Utc))
}

impl AiMemoryAdapter<MemoryTransport> {
    /// Spawn the server and connect.
    ///
    /// Roots the substrate's wiki and index at `data_dir`, which keeps
    /// Orqyn's pages out of the operator's real memory. Performs no other
    /// I/O: the project is created by the first write, and reads of a project
    /// that does not exist fail closed (see [`Self::new`]).
    pub async fn connect(
        binary: &str,
        data_dir: &std::path::Path,
        workspace: impl Into<String>,
        project: impl Into<String>,
    ) -> Result<Self, AiMemoryAdapterError> {
        let transport = MemoryTransport::spawn(binary, data_dir).await?;
        Ok(AiMemoryAdapter::new(transport, workspace, project))
    }
}

#[async_trait]
impl<T: AiMemoryWire> Provider for AiMemoryAdapter<T> {
    type Error = AiMemoryAdapterError;
}

#[async_trait]
impl<T: AiMemoryWire> MemoryProvider for AiMemoryAdapter<T> {
    async fn save_memory(&self, memory: Memory) -> Result<Memory, Self::Error> {
        // The title goes in as a markdown H1 rather than the `title` argument:
        // the substrate derives the title from it, and the argument form is a
        // known source of JSON-escaping failures when the title has quotes or
        // colons. Deriving the same way the substrate does makes the round
        // trip exact.
        let body = format!("# {}\n\n{}", memory.title, memory.body);

        let mut arguments = json!({
            "path": memory.id,
            "body": body,
            "tags": memory.tags,
        });
        // Orqyn's `kind` is coarse; the substrate's tier is a retention
        // class. Only a kind that names a real tier is forwarded: the substrate
        // rejects an unknown tier, and inventing a retention class for a kind
        // that is not one would misfile the page. A kind that is not a tier is
        // therefore dropped, and the page's tier is then whatever the substrate
        // defaulted to — which the read-back below reports honestly rather than
        // echoing back what Orqyn was asked to store.
        if let Some(tier) = memory
            .kind
            .as_deref()
            .filter(|kind| KNOWN_TIERS.contains(kind))
        {
            arguments["tier"] = json!(tier);
        }

        let written: WritePageReply = self.call_json("memory_write_page", arguments).await?;

        // Read back the canonical record: the substrate may normalize the path,
        // derive a different title than the one intended, or reject the write
        // outright, and the reply the caller gets should be what is stored.
        let page =
            self.fetch_page(&written.path)
                .await?
                .ok_or_else(|| AiMemoryAdapterError::Tool {
                    tool: "memory_read_page",
                    message: format!(
                        "a page just written to {} is not readable in its own scope",
                        written.path
                    ),
                })?;

        // The write path reports no rank meaning of its own, so the record
        // Orqyn hands back claims neither a relevance score nor a change
        // time.
        Ok(self.page_to_memory(page, RankMeaning::None))
    }

    async fn query_memories(&self, query: &MemoryQuery) -> Result<Vec<Memory>, Self::Error> {
        // `tags` are deliberately not forwarded: this substrate's query has no
        // tag filter, and the trait documents tags as a best-effort restriction.
        let response: MemoryQueryResponse = self
            .call_json(
                "memory_query",
                json!({
                    "query": query.text,
                    "limit": query.limit.min(100) as usize,
                }),
            )
            .await?;

        let mut memories = Vec::with_capacity(response.hits.len());
        for hit in response.hits {
            // Read the rank before the hit moves into `materialize`.
            let rank = hit.rank;
            if let Some(memory) = self.materialize(hit, RankMeaning::Relevance(rank)).await? {
                memories.push(memory);
            }
        }
        Ok(memories)
    }

    async fn recent_memories(&self, limit: u32) -> Result<Vec<Memory>, Self::Error> {
        let response: MemoryRecentResponse = self
            .call_json("memory_recent", json!({ "limit": limit.min(100) as usize }))
            .await?;

        let mut memories = Vec::with_capacity(response.hits.len());
        for hit in response.hits {
            // Here, and only here, `rank` is the page's change time in
            // microseconds — the one read path where `updated_at` is real.
            let rank = hit.rank;
            if let Some(memory) = self.materialize(hit, RankMeaning::Recent(rank)).await? {
                memories.push(memory);
            }
        }
        Ok(memories)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A fake ai-memory: a set of pages keyed by path.
    ///
    /// It records what the adapter sent, so a test asserts on the actual
    /// request rather than on what the adapter claims it would do, and it
    /// serves realistic reply JSON so the wire mirrors are exercised against
    /// the shapes the live server emits.
    struct FakeAiMemory {
        pages: HashMap<String, FakePage>,
        /// Every call, in order: (tool, arguments).
        calls: Vec<(String, Value)>,
    }

    #[derive(Debug, Clone)]
    struct FakePage {
        title: String,
        body: String,
        tags: Vec<String>,
        /// The retention tier the page was written with.
        tier: Option<String>,
        /// When the fake generated this version, as an RFC3339 string — the
        /// shape of the substrate's `generated.at` frontmatter stamp.
        generated_at: String,
        /// Change time in microseconds, as `memory_recent` reports it.
        updated_at_us: i64,
    }

    impl FakeAiMemory {
        fn new() -> Self {
            FakeAiMemory {
                pages: HashMap::new(),
                calls: Vec::new(),
            }
        }

        /// A fixed change time, in microseconds, that the fake reports for
        /// every page — enough to exercise the adapter's timestamp reading
        /// without a clock.
        const UPDATED_AT_US: i64 = 1_758_950_400_000_000;

        /// The instant the fake stamps on every page, mirroring the
        /// `generated.at` frontmatter the real substrate writes.
        const GENERATED_AT: &'static str = "2026-09-27T04:28:24Z";

        fn read_reply(&self, path: &str) -> Value {
            let page = self.pages.get(path).expect("page exists");
            // Frontmatter accumulates the way the real substrate's does: tags
            // when the caller sent any, the tier the page was written with, and
            // the generation stamp always.
            let mut frontmatter = serde_json::json!({
                "type": "Note",
                "generated": { "by": "fake", "at": page.generated_at },
            });
            if !page.tags.is_empty() {
                frontmatter["tags"] = serde_json::json!(page.tags);
            }
            if let Some(tier) = &page.tier {
                frontmatter["tier"] = serde_json::json!(tier);
            }
            serde_json::json!({
                "path": path,
                "title": page.title,
                "body": page.body,
                "frontmatter": frontmatter,
            })
        }
    }

    #[async_trait]
    impl AiMemoryWire for std::sync::Mutex<FakeAiMemory> {
        async fn call_tool(
            &mut self,
            name: &str,
            arguments: Value,
        ) -> Result<String, AiMemoryAdapterError> {
            // Exclusive access is already held (&mut self), so get_mut reaches
            // the state without taking the lock.
            let fake = self.get_mut().expect("fake poisoned");
            fake.calls.push((name.to_string(), arguments.clone()));

            match name {
                "memory_query" => {
                    let needle = arguments
                        .get("query")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_lowercase();
                    let limit =
                        arguments.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
                    // A lexical stand-in for FTS5: every word must appear in the
                    // body or the title. Ranking is a stable irrelevant
                    // constant, so the adapter's negation is verifiable without
                    // a real ranker.
                    let hits: Vec<Value> = fake
                        .pages
                        .iter()
                        .filter(|(_, page)| {
                            let hay = format!("{} {}", page.title, page.body).to_lowercase();
                            needle
                                .split_whitespace()
                                .all(|word| hay.contains(word))
                        })
                        .take(limit)
                        .map(|(path, page)| {
                            serde_json::json!({
                                "id": "01J6Z4KQ4N",
                                "path": path,
                                "title": page.title,
                                "snippet": format!("…{}…", page.body.chars().take(20).collect::<String>()),
                                "rank": -4.5,
                            })
                        })
                        .collect();
                    Ok(serde_json::json!({ "hits": hits }).to_string())
                }
                "memory_recent" => {
                    let limit =
                        arguments.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
                    let mut pages: Vec<(&String, &FakePage)> = fake.pages.iter().collect();
                    pages.sort_by_key(|(_, page)| std::cmp::Reverse(page.updated_at_us));
                    let hits = pages
                        .into_iter()
                        .take(limit)
                        .map(|(path, page)| {
                            serde_json::json!({
                                "id": "01J6Z4KQ4N",
                                "path": path,
                                "title": page.title,
                                "snippet": "…",
                                // The overloaded slot, holding microseconds here.
                                "rank": page.updated_at_us,
                            })
                        })
                        .collect::<Vec<_>>();
                    Ok(serde_json::json!({ "hits": hits }).to_string())
                }
                "memory_read_page" => {
                    let path = arguments.get("path").and_then(Value::as_str).unwrap_or("");
                    fake.pages
                        .get(path)
                        .map(|_| fake.read_reply(path))
                        .map(|value| value.to_string())
                        .ok_or_else(|| AiMemoryAdapterError::Tool {
                            tool: "memory_read_page",
                            message: format!("no pages found for path {path:?}"),
                        })
                }
                "memory_write_page" => {
                    let path = arguments
                        .get("path")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let body = arguments
                        .get("body")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    // Derive the title the way the substrate does: the first H1,
                    // falling back to the path stem.
                    let title = body
                        .lines()
                        .find_map(|line| line.strip_prefix("# ").map(str::to_string))
                        .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(&path).to_string());
                    let tags = arguments
                        .get("tags")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect::<Vec<_>>();
                    // A write that omits the tier resets it to the substrate's
                    // default — the behavior that makes a non-tier kind not
                    // round-trip, verified against the live server.
                    let tier = arguments
                        .get("tier")
                        .and_then(Value::as_str)
                        .map(String::from);
                    fake.pages.insert(
                        path.clone(),
                        FakePage {
                            title,
                            body,
                            tags,
                            tier,
                            generated_at: FakeAiMemory::GENERATED_AT.to_string(),
                            updated_at_us: FakeAiMemory::UPDATED_AT_US,
                        },
                    );
                    Ok(serde_json::json!({
                        "page_id": "01J6Z4KQ4N",
                        "path": path,
                        "checkpoint": "abc123",
                    })
                    .to_string())
                }
                other => Err(AiMemoryAdapterError::Tool {
                    tool: "unknown",
                    message: format!("fake has no {other}"),
                }),
            }
        }
    }

    fn adapter() -> AiMemoryAdapter<std::sync::Mutex<FakeAiMemory>> {
        AiMemoryAdapter::new(
            std::sync::Mutex::new(FakeAiMemory::new()),
            "director-workspace",
            "director-project",
        )
    }

    fn memory(id: &str, title: &str, body: &str) -> Memory {
        Memory {
            id: id.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            kind: None,
            tags: vec![],
            score: None,
            updated_at: None,
        }
    }

    #[tokio::test]
    async fn saving_a_memory_round_trips_its_path_and_title() {
        let adapter = adapter();
        let saved = adapter
            .save_memory(memory(
                "notes/auth.md",
                "Auth design",
                "we chose stateless JWTs",
            ))
            .await
            .expect("save");

        assert_eq!(saved.id, "notes/auth.md");
        assert_eq!(saved.title, "Auth design");
        assert_eq!(saved.body, "# Auth design\n\nwe chose stateless JWTs");
    }

    #[tokio::test]
    async fn the_title_is_sent_as_a_markdown_h1_not_as_an_argument() {
        // The substrate asks callers to prefer the H1 over the `title`
        // argument, which mangles titles containing punctuation. Asserting on
        // the request shows the choice was made on the wire.
        let adapter = adapter();
        adapter
            .save_memory(memory("notes/x.md", "Why 2 + 2 = 4: a proof", "body"))
            .await
            .expect("save");

        let transport = adapter.transport.lock().await;
        let fake = transport.lock().unwrap();
        let write = fake
            .calls
            .iter()
            .find(|(tool, _)| tool == "memory_write_page")
            .expect("a write was sent");
        assert!(
            write.1.get("title").is_none(),
            "the title argument was not sent"
        );
        assert_eq!(
            write.1.get("body").and_then(Value::as_str),
            Some("# Why 2 + 2 = 4: a proof\n\nbody")
        );
    }

    #[tokio::test]
    async fn a_known_kind_becomes_a_tier_but_an_unknown_one_is_dropped() {
        let adapter = adapter();
        let mut known = memory("notes/a.md", "A", "body");
        known.kind = Some("episodic".into());
        adapter.save_memory(known).await.expect("save");

        let mut unknown = memory("notes/b.md", "B", "body");
        unknown.kind = Some("decision".into());
        adapter.save_memory(unknown).await.expect("save");

        // The read-back: a kind that named a tier is the kind Orqyn sees
        // again, because the substrate stamps the tier into frontmatter. A kind
        // that did not is gone, and the tier reported is the substrate's
        // default — not the kind Orqyn was asked to store. Fetched before
        // the lock below so no await is held across it.
        let back = adapter.recent_memories(10).await.expect("recent");
        let a = back
            .iter()
            .find(|memory| memory.id == "notes/a.md")
            .expect("page a was written");
        assert_eq!(a.kind.as_deref(), Some("episodic"), "the tier round-trips");

        let transport = adapter.transport.lock().await;
        let fake = transport.lock().unwrap();
        let writes: Vec<_> = fake
            .calls
            .iter()
            .filter(|(tool, _)| tool == "memory_write_page")
            .collect();
        let tier_of = |path: &str| {
            writes
                .iter()
                .find(|(_, args)| args.get("path").and_then(Value::as_str) == Some(path))
                .and_then(|(_, args)| args.get("tier").and_then(Value::as_str))
        };
        assert_eq!(tier_of("notes/a.md"), Some("episodic"));
        assert_eq!(
            tier_of("notes/b.md"),
            None,
            "a kind that is not a tier is not invented into one"
        );
    }

    #[tokio::test]
    async fn tags_round_trip_through_frontmatter() {
        let adapter = adapter();
        let mut memory = memory("notes/db.md", "DB schema", "the users table");
        memory.tags = vec!["db".into(), "decision".into()];
        let saved = adapter.save_memory(memory).await.expect("save");

        let read_back = adapter
            .query_memories(&MemoryQuery::new("users table", 10))
            .await
            .expect("query");
        assert_eq!(read_back.len(), 1);
        assert_eq!(read_back[0].tags, saved.tags);
    }

    #[tokio::test]
    async fn query_returns_full_bodies_not_snippets() {
        let adapter = adapter();
        adapter
            .save_memory(memory(
                "notes/auth.md",
                "Auth design",
                "we chose stateless JWTs for login",
            ))
            .await
            .expect("save");

        let hits = adapter
            .query_memories(&MemoryQuery::new("stateless JWTs", 10))
            .await
            .expect("query");
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].body.contains("we chose stateless JWTs for login"),
            "the body is the full page, not the FTS5 snippet"
        );
    }

    #[tokio::test]
    async fn a_query_hit_reports_relevance_and_the_frontmatter_change_time() {
        // Two sources, kept distinct: the hit's `rank` is a relevance score,
        // and the change time comes from the fully-read page's frontmatter
        // stamp — the only timestamp available on the query path.
        let adapter = adapter();
        adapter
            .save_memory(memory("notes/a.md", "A", "rust async runtime"))
            .await
            .expect("save");

        let hits = adapter
            .query_memories(&MemoryQuery::new("rust async", 10))
            .await
            .expect("query");
        assert_eq!(hits.len(), 1);
        // The fake ranks at -4.5; the adapter negates to +4.5.
        assert_eq!(hits[0].score, Some(4.5));
        assert_eq!(
            hits[0].updated_at,
            Some(
                chrono::DateTime::parse_from_rfc3339(FakeAiMemory::GENERATED_AT)
                    .expect("the fake stamps a valid instant")
                    .with_timezone(&chrono::Utc)
            ),
            "the change time is the frontmatter generation stamp"
        );
    }

    #[tokio::test]
    async fn a_recent_hit_reports_a_change_time_but_no_relevance() {
        // The other half of the split: the same field, holding microseconds.
        let adapter = adapter();
        adapter
            .save_memory(memory("notes/a.md", "A", "body"))
            .await
            .expect("save");

        let hits = adapter.recent_memories(10).await.expect("recent");
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].updated_at,
            chrono::DateTime::from_timestamp_micros(FakeAiMemory::UPDATED_AT_US),
            "the recent rank is the change time in microseconds"
        );
        assert_eq!(hits[0].score, None, "a recent hit has no relevance score");
    }

    #[tokio::test]
    async fn recent_memories_respects_the_limit() {
        let adapter = adapter();
        for n in 0..5 {
            adapter
                .save_memory(memory(
                    &format!("notes/{n}.md"),
                    &format!("Item {n}"),
                    "body",
                ))
                .await
                .expect("save");
        }
        assert_eq!(adapter.recent_memories(2).await.expect("recent").len(), 2);
    }

    #[tokio::test]
    async fn query_limit_is_forwarded_and_capped() {
        let adapter = adapter();
        adapter
            .save_memory(memory("notes/a.md", "A", "shared token here"))
            .await
            .expect("save");

        adapter
            .query_memories(&MemoryQuery::new("shared token", 10))
            .await
            .expect("query");

        async fn last_query_limit(
            adapter: &AiMemoryAdapter<std::sync::Mutex<FakeAiMemory>>,
        ) -> u64 {
            let transport = adapter.transport.lock().await;
            let fake = transport.lock().unwrap();
            fake.calls
                .iter()
                .rev()
                .find(|(tool, _)| tool == "memory_query")
                .and_then(|(_, args)| args.get("limit").and_then(Value::as_u64))
                .expect("a query was sent")
        }

        assert_eq!(last_query_limit(&adapter).await, 10);

        // The substrate rejects a limit over 100; the adapter clamps rather
        // than letting the call fail.
        adapter
            .query_memories(&MemoryQuery::new("shared token", 500))
            .await
            .expect("query with a clamped limit");
        assert_eq!(last_query_limit(&adapter).await, 100);
    }

    #[tokio::test]
    async fn every_call_carries_the_workspace_and_project() {
        // The scoping rule: a static client must send both, on every call, or
        // the substrate resolves scope some other way.
        let adapter = adapter();
        adapter
            .save_memory(memory("notes/a.md", "A", "body"))
            .await
            .expect("save");
        adapter
            .query_memories(&MemoryQuery::new("body", 5))
            .await
            .expect("query");
        adapter.recent_memories(5).await.expect("recent");

        let transport = adapter.transport.lock().await;
        let fake = transport.lock().unwrap();
        for (tool, args) in &fake.calls {
            assert_eq!(
                args.get("workspace").and_then(Value::as_str),
                Some("director-workspace"),
                "{tool} sent the workspace"
            );
            assert_eq!(
                args.get("project").and_then(Value::as_str),
                Some("director-project"),
                "{tool} sent the project"
            );
        }
    }

    #[tokio::test]
    async fn a_hit_whose_page_vanished_is_skipped_not_snippeted() {
        let adapter = adapter();
        // Arrange a hit with no page behind it: the fake's query reads the page
        // map, so remove the page after writing it and re-run the query.
        adapter
            .save_memory(memory("notes/gone.md", "Gone", "unique searchable words"))
            .await
            .expect("save");
        {
            let transport = adapter.transport.lock().await;
            transport.lock().unwrap().pages.remove("notes/gone.md");
        }
        assert!(
            adapter
                .query_memories(&MemoryQuery::new("unique searchable words", 10))
                .await
                .expect("query")
                .is_empty(),
            "a hit with no readable page is skipped, not returned as a snippet"
        );
    }

    #[tokio::test]
    async fn an_unparsed_reply_is_reported_as_malformed_not_as_success() {
        // If a wire mirror drifts from the live server, the failure must be
        // loud. The fake serves a shape the query reply cannot parse.
        struct Broken;
        #[async_trait]
        impl AiMemoryWire for std::sync::Mutex<Broken> {
            async fn call_tool(
                &mut self,
                _name: &str,
                _arguments: Value,
            ) -> Result<String, AiMemoryAdapterError> {
                // No `hits` key.
                Ok(r#"{"total": 0}"#.to_string())
            }
        }
        let adapter = AiMemoryAdapter::<std::sync::Mutex<Broken>>::new(
            std::sync::Mutex::new(Broken),
            "ws",
            "proj",
        );
        assert!(matches!(
            adapter.query_memories(&MemoryQuery::new("x", 5)).await,
            Err(AiMemoryAdapterError::Malformed { .. })
        ));
    }
}
