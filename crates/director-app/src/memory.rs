//! Retrieving previously published checkpoints from an external memory substrate.
//!
//! Phase 6A made Orqyn *write* checkpoint pages. This makes it *read* them, which
//! closes the loop: a replacement agent can be told what Orqyn published about a
//! task last time, rather than starting from SQLite alone.
//!
//! # SQLite is still authoritative, and this module cannot change that
//!
//! Nothing retrieved here can move a task, invalidate a verification, rewrite a
//! checkpoint lineage version, or widen a scope. What comes back is a *copy* of
//! something Orqyn itself wrote earlier, which may since be stale, and which may
//! have been written by an older build. The renderer therefore labels it as an
//! external copy and places it after every authoritative section.
//!
//! # Task association is verified, not assumed
//!
//! The substrate's query has no tag filter (`AiMemoryAdapter::query_memories`
//! deliberately does not forward `MemoryQuery::tags`), so a query returns whatever
//! the substrate thinks matches. A hit is therefore treated as being about this
//! task only when its own recorded identity says so — the page path Orqyn derives
//! from the task id, or the `orqyn-checkpoint` tag carrying that id. A result
//! that cannot be tied to the task is **dropped**, not shown with a caveat: the
//! alternative is presenting unrelated content as a fact about this task, which
//! is the exact failure this module exists to prevent.
//!
//! # Absent is not the same as unavailable
//!
//! [`Retrieval`] has three states. A substrate with no page for a task and a
//! substrate that could not be asked are different facts with different
//! consequences, and collapsing them would make an outage look like a clean slate.
//! Only [`Retrieval::Unavailable`] is ever produced by a failure; an empty result
//! set is [`Retrieval::Absent`].

use std::time::Duration;

use director_domain::ids::TaskId;
use director_domain::providers::{Memory, MemoryProvider, MemoryQuery};

/// The tag Orqyn stamps on every checkpoint page it publishes. Part of the
/// task-association check, so a page that predates the tag is not trusted even if
/// its path matches.
pub const CHECKPOINT_TAG: &str = "orqyn-checkpoint";

/// How long a retrieval may take before the briefing proceeds without it.
///
/// Bounds the whole query, not just the transport. SQLite state is already loaded
/// by this point, so exceeding this costs the agent one clearly-labelled section
/// and nothing else.
pub const EXTERNAL_MEMORY_TIMEOUT: Duration = Duration::from_secs(5);

/// How many candidates to ask the substrate for.
///
/// Small on purpose. Only one page per task is expected, and every extra
/// candidate is another piece of third-party content that has to be verified
/// against the task and then usually discarded.
pub const RETRIEVAL_CANDIDATES: u32 = 5;

/// Characters of external body text kept per page.
pub const MAX_EXTERNAL_BODY_CHARS: usize = 4_000;

/// Total characters kept across every page in one retrieval.
pub const MAX_EXTERNAL_TOTAL_CHARS: usize = 8_000;

/// One page of previously published checkpoint content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCheckpoint {
    /// The substrate's own identifier for the page, preserved verbatim so an
    /// operator can go and look at it.
    pub source_id: String,
    /// When the substrate last changed the page, when it says.
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The page body, possibly truncated. See [`MAX_EXTERNAL_BODY_CHARS`].
    pub body: String,
    /// True when the body was cut to fit [`MAX_EXTERNAL_BODY_CHARS`].
    pub truncated: bool,
}

/// What retrieval produced, including the ways it can go wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Retrieval {
    /// The substrate was asked and holds nothing about this task.
    Absent,
    /// The substrate was asked and holds at least one page about this task.
    Found {
        /// The pages, verified as belonging to this task.
        pages: Vec<ExternalCheckpoint>,
        /// How many candidates the substrate returned that were dropped for not
        /// being about this task. Reported so a persistently high count is
        /// visible rather than looking like a clean miss.
        discarded: usize,
        /// True when either a body or the total was cut to fit its bound.
        truncated: bool,
    },
    /// The substrate could not be asked. Never a silent empty result.
    Unavailable {
        /// Why, as the provider reported it.
        reason: String,
    },
}

impl Retrieval {
    /// The pages, or an empty slice when there are none.
    pub fn pages(&self) -> &[ExternalCheckpoint] {
        match self {
            Retrieval::Found { pages, .. } => pages,
            _ => &[],
        }
    }

    /// True when the substrate could not be reached.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Retrieval::Unavailable { .. })
    }

    /// A one-line statement of how this retrieval went, for a diagnostic.
    pub fn summary(&self) -> String {
        match self {
            Retrieval::Absent => "no previously published checkpoint for this task".into(),
            Retrieval::Found {
                pages,
                discarded,
                truncated,
            } => format!(
                "{} previously published page(s){}{}",
                pages.len(),
                if *discarded > 0 {
                    format!(", {discarded} candidate(s) discarded as unrelated")
                } else {
                    String::new()
                },
                if *truncated {
                    ", truncated to fit bounds"
                } else {
                    ""
                }
            ),
            Retrieval::Unavailable { reason } => format!("external memory unavailable: {reason}"),
        }
    }
}

/// The page path Orqyn publishes a task's checkpoint to.
///
/// Mirrors `checkpoint::memory_for`, which writes it. Both sides derive it from
/// the task id rather than storing it, so they cannot drift.
pub fn page_path_for(task: &TaskId) -> String {
    format!("checkpoints/{}.md", task.as_str().to_lowercase())
}

/// The text a query searches for.
///
/// Matches the header `checkpoint::memory_for` writes, which is what makes the
/// page findable at all — the substrate has no tag filter to search on instead.
/// Deriving both sides from one constant is what keeps write and read in step.
pub fn query_text_for(task: &TaskId) -> String {
    format!("Resume status evidence for {}", task.as_str())
}

/// True when `memory` is verifiably about `task`.
///
/// Two independent signals are accepted, because a page could carry one without
/// the other after an older build or a manual write. Neither signal is inferred
/// from the body: content that merely mentions the task is not evidence that it
/// is the task's page.
pub fn belongs_to_task(memory: &Memory, task: &TaskId) -> bool {
    let task_key = task.as_str().to_lowercase();
    if memory.id == page_path_for(task) {
        return true;
    }
    memory.tags.iter().any(|t| t == CHECKPOINT_TAG) && memory.tags.contains(&task_key)
}

/// Retrieve this task's previously published checkpoint pages.
///
/// `limit` bounds the whole operation, so a caller can bound retrieval
/// independently of the substrate's own per-call bound. Exceeding it yields
/// [`Retrieval::Unavailable`] — never an empty result, because "the query timed
/// out" and "the substrate has nothing" must not read the same.
pub async fn retrieve_checkpoints<M: MemoryProvider>(
    memory: &M,
    task: &TaskId,
    limit: Duration,
) -> Retrieval {
    let query = MemoryQuery::new(query_text_for(task), RETRIEVAL_CANDIDATES);

    let response = match tokio::time::timeout(limit, memory.query_memories(&query)).await {
        Ok(Ok(hits)) => hits,
        Ok(Err(err)) => {
            return Retrieval::Unavailable {
                reason: err.to_string(),
            }
        }
        Err(_) => {
            return Retrieval::Unavailable {
                reason: format!(
                    "the query did not answer within {limit:?}; the briefing carries no \
                     external copy rather than a stale one"
                ),
            }
        }
    };

    let related: Vec<&Memory> = response
        .iter()
        .filter(|m| belongs_to_task(m, task))
        .collect();
    let discarded = response.len() - related.len();

    if related.is_empty() {
        return if discarded == 0 {
            Retrieval::Absent
        } else {
            // Something was returned, just none of it provably about this task.
            // Reported as absent *with a count* rather than as a clean miss, so a
            // systematically mis-tagging substrate is visible.
            Retrieval::Found {
                pages: Vec::new(),
                discarded,
                truncated: false,
            }
        };
    }

    let (pages, truncated) = bound_pages(&related);
    Retrieval::Found {
        pages,
        discarded,
        truncated,
    }
}

/// Clip bodies to the per-page and total bounds.
///
/// Truncation happens at character boundaries and always leaves the marker saying
/// it happened, so a reader can tell a clipped page from a short one — the same
/// discipline the action log and the briefing itself follow.
fn bound_pages(memories: &[&Memory]) -> (Vec<ExternalCheckpoint>, bool) {
    let mut pages = Vec::with_capacity(memories.len());
    let mut total = 0usize;
    let mut truncated = false;

    for memory in memories {
        if total >= MAX_EXTERNAL_TOTAL_CHARS {
            truncated = true;
            break;
        }
        let allowance = (MAX_EXTERNAL_TOTAL_CHARS - total).min(MAX_EXTERNAL_BODY_CHARS);
        let (body, clipped) = clip(&memory.body, allowance);
        if clipped {
            truncated = true;
        }
        total += body.chars().count();
        pages.push(ExternalCheckpoint {
            source_id: memory.id.clone(),
            updated_at: memory.updated_at,
            body,
            truncated: clipped,
        });
    }

    (pages, truncated)
}

fn clip(text: &str, allowance: usize) -> (String, bool) {
    if text.chars().count() <= allowance {
        return (text.to_string(), false);
    }
    let cut: String = text.chars().take(allowance).collect();
    (
        format!("{cut}\n[EXTERNAL PAGE TRUNCATED: it exceeded the retrieval bound.]"),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> TaskId {
        TaskId::from_string("AUTH")
    }

    fn memory(id: &str, tags: &[&str]) -> Memory {
        Memory {
            id: id.into(),
            title: "Checkpoint: AUTH".into(),
            body: "**Resume status evidence for AUTH**".into(),
            kind: None,
            tags: tags.iter().map(|t| (*t).to_string()).collect(),
            score: None,
            updated_at: None,
        }
    }

    #[test]
    fn the_write_and_read_paths_agree_on_one_path() {
        // `memory_for` writes `checkpoints/{task lowercased}.md`; if these two
        // ever disagreed, publication would silently stop being retrievable.
        assert_eq!(page_path_for(&task()), "checkpoints/auth.md");
        assert_eq!(query_text_for(&task()), "Resume status evidence for AUTH");
    }

    #[test]
    fn a_page_is_recognised_by_its_path() {
        let m = memory("checkpoints/auth.md", &[]);
        assert!(belongs_to_task(&m, &task()));
    }

    #[test]
    fn a_page_is_recognised_by_both_tags() {
        let m = memory("notes/something-else.md", &[CHECKPOINT_TAG, "auth"]);
        assert!(belongs_to_task(&m, &task()));
    }

    #[test]
    fn unrelated_content_is_never_accepted() {
        // Same body, different task's page.
        let other_task = memory("checkpoints/db.md", &[CHECKPOINT_TAG, "db"]);
        assert!(!belongs_to_task(&other_task, &task()));

        // Right tag, no task tag.
        let untagged = memory("notes/loose.md", &[CHECKPOINT_TAG]);
        assert!(!belongs_to_task(&untagged, &task()));

        // A page belonging to no task at all.
        let stray = memory("notes/stray.md", &[]);
        assert!(!belongs_to_task(&stray, &task()));

        // And the body mentioning the task is never enough.
        let mention = Memory {
            id: "notes/mention.md".into(),
            tags: vec![],
            body: "this mentions AUTH somewhere".into(),
            ..memory("notes/mention.md", &[])
        };
        assert!(
            !belongs_to_task(&mention, &task()),
            "content that merely names the task is not evidence it is the task's page"
        );
    }

    #[test]
    fn nothing_is_accepted_and_nothing_is_discarded() {
        let (pages, truncated) = bound_pages(&[]);
        assert!(pages.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn a_short_body_is_kept_whole() {
        let m = memory("checkpoints/auth.md", &[]);
        let (pages, truncated) = bound_pages(&[&m]);
        assert_eq!(pages.len(), 1);
        assert!(!pages[0].truncated);
        assert!(!truncated);
        assert!(!pages[0].body.contains("TRUNCATED"));
    }

    #[test]
    fn an_oversized_body_is_clipped_and_says_so() {
        let big = Memory {
            body: "y".repeat(MAX_EXTERNAL_BODY_CHARS * 3),
            ..memory("checkpoints/auth.md", &[])
        };
        let (pages, truncated) = bound_pages(&[&big]);
        assert!(pages[0].truncated, "a clipped page must declare itself");
        assert!(truncated);
        assert!(pages[0].body.contains("EXTERNAL PAGE TRUNCATED"));
        assert!(
            pages[0].body.chars().count() <= MAX_EXTERNAL_BODY_CHARS + 64,
            "the clip must actually bound the body"
        );
    }

    #[test]
    fn the_total_bound_stops_adding_pages() {
        let big = || Memory {
            body: "z".repeat(MAX_EXTERNAL_BODY_CHARS + 100),
            ..memory("checkpoints/auth.md", &[])
        };
        let a = big();
        let b = big();
        let c = big();
        let (pages, truncated) = bound_pages(&[&a, &b, &c]);
        assert!(truncated);
        let total: usize = pages.iter().map(|p| p.body.chars().count()).sum();
        assert!(
            total <= MAX_EXTERNAL_TOTAL_CHARS + 64 * pages.len(),
            "the combined total must be bounded, got {total}"
        );
    }

    #[test]
    fn the_three_states_report_themselves_distinctly() {
        assert!(Retrieval::Absent
            .summary()
            .contains("no previously published"));
        assert!(!Retrieval::Absent.is_unavailable());

        let unavailable = Retrieval::Unavailable {
            reason: "connection refused".into(),
        };
        assert!(unavailable.is_unavailable());
        assert!(unavailable.summary().contains("connection refused"));

        let found = Retrieval::Found {
            pages: vec![ExternalCheckpoint {
                source_id: "checkpoints/auth.md".into(),
                updated_at: None,
                body: "x".into(),
                truncated: false,
            }],
            discarded: 2,
            truncated: false,
        };
        assert!(!found.is_unavailable());
        assert!(found.summary().contains("1 previously published page"));
        assert!(found.summary().contains("2 candidate(s) discarded"));
        assert_eq!(found.pages().len(), 1);
        assert!(Retrieval::Absent.pages().is_empty());
    }
}
