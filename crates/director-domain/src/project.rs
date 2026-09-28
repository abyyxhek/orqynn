//! [Project] — the thing all the work belongs to.
//!
//! A Project is deliberately thin: it is a *root*, not a container. Director
//! does not store tasks inside a project object — tasks live in the substrate
//! and are referenced by id. The project exists to anchor ids, scopes, and
//! per-project defaults, and to answer the one question that matters when
//! multiple projects are open: "which working directory is this?"

use serde::{Deserialize, Serialize};

use crate::ids::{ProjectId, RepositoryId};

/// Default branch convention used for staleness comparisons when a repository
/// has no clear trunk. Recorded per-project so a team on `develop` or `trunk`
/// does not have to configure it per task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultBranch {
    /// `main`.
    Main,
    /// `master` — legacy repositories.
    Master,
    /// An explicitly named branch.
    Named(String),
}

impl std::fmt::Display for DefaultBranch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DefaultBranch::Main => f.write_str("main"),
            DefaultBranch::Master => f.write_str("master"),
            DefaultBranch::Named(n) => f.write_str(n),
        }
    }
}

/// A unit of ownership for work: one repository (or one working tree), one set
/// of tasks, one plan at a time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// This project's identifier.
    pub id: ProjectId,
    /// Human-facing name, e.g. "checkout-service".
    pub name: String,
    /// Absolute path to the working tree Director observes. This is what makes
    /// project state observable rather than remembered.
    pub root: String,
    /// Branch Director compares checkpoints against when nothing more specific
    /// is known.
    pub default_branch: DefaultBranch,
    /// The repository Director observes for this project, once one has been
    /// registered. A reference, not an embedded copy: the [`crate::repository::Repository`]
    /// record itself belongs to the git observation layer, which owns the git
    /// detail. Director's store keeps only the link.
    pub repository_id: Option<RepositoryId>,
    /// Monotonic version for optimistic concurrency. Bumped exactly once per
    /// change that matters — see [`Project::bump_state_version`]. New records
    /// start at 1.
    ///
    /// This is the same mechanism as [`crate::repository::ProjectStateSnapshot::state_version`]:
    /// one version per mutable object, no competing counters.
    #[serde(default = "default_state_version")]
    pub state_version: u64,
    /// When Director first registered it.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When it last changed.
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// The version a freshly persisted record starts on. One, not zero: a record
/// that exists has been written once.
fn default_state_version() -> u64 {
    1
}

impl Project {
    /// Register a project rooted at `root`, defaulting to `main`.
    pub fn new(id: ProjectId, name: impl Into<String>, root: impl Into<String>) -> Self {
        let now = chrono::Utc::now();
        Project {
            id,
            name: name.into(),
            root: root.into(),
            default_branch: DefaultBranch::Main,
            repository_id: None,
            state_version: default_state_version(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Record a change, stamping `updated_at`.
    pub fn touch(&mut self) {
        self.updated_at = chrono::Utc::now();
    }

    /// Advance the optimistic-concurrency version. Called by the store on a
    /// successful update, not by callers directly.
    pub fn bump_state_version(&mut self) {
        self.state_version += 1;
        self.touch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_project_defaults_to_main() {
        let p = Project::new(
            ProjectId::from_string("PROJ-checkout"),
            "checkout-service",
            "/repo/checkout",
        );
        assert_eq!(p.default_branch.to_string(), "main");
        assert_eq!(p.name, "checkout-service");
    }

    #[test]
    fn named_default_branches_display_verbatim() {
        assert_eq!(DefaultBranch::Named("trunk".into()).to_string(), "trunk");
        assert_eq!(DefaultBranch::Master.to_string(), "master");
    }

    #[test]
    fn project_round_trips_through_serde() {
        let p = Project::new(
            ProjectId::from_string("PROJ-checkout"),
            "checkout-service",
            "/repo/checkout",
        );
        let json = serde_json::to_string(&p).unwrap();
        let back: Project = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn a_new_project_starts_at_version_one() {
        let p = Project::new(
            ProjectId::from_string("PROJ-checkout"),
            "checkout-service",
            "/repo/checkout",
        );
        assert_eq!(p.state_version, 1);
        assert!(p.repository_id.is_none());
    }

    #[test]
    fn bumping_the_version_advances_it_by_one_and_stamps_the_time() {
        let mut p = Project::new(
            ProjectId::from_string("PROJ-checkout"),
            "checkout-service",
            "/repo/checkout",
        );
        let before = p.updated_at;
        std::thread::sleep(std::time::Duration::from_millis(2));
        p.bump_state_version();
        assert_eq!(p.state_version, 2);
        assert!(p.updated_at > before);
    }

    #[test]
    fn a_project_references_its_repository_by_id_only() {
        let mut p = Project::new(
            ProjectId::from_string("PROJ-checkout"),
            "checkout-service",
            "/repo/checkout",
        );
        assert!(p.repository_id.is_none());
        p.repository_id = Some(RepositoryId::from_string("REPO-checkout"));
        // The reference round-trips; the repository record itself is not inlined.
        let json = serde_json::to_string(&p).unwrap();
        let back: Project = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.repository_id,
            Some(RepositoryId::from_string("REPO-checkout"))
        );
    }

    #[test]
    fn a_pre_phase4_project_payload_still_deserializes() {
        // A payload written before Phase 4 has no `state_version` and no
        // `repository_id`. Both default, so the record still reads.
        let legacy = serde_json::json!({
            "id": "PROJ-checkout",
            "name": "checkout-service",
            "root": "/repo/checkout",
            "default_branch": "main",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        });
        let parsed: Project = serde_json::from_value(legacy).unwrap();
        assert_eq!(parsed.state_version, 1);
        assert!(parsed.repository_id.is_none());
    }
}
