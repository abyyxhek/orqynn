//! [Project] — the thing all the work belongs to.
//!
//! A Project is deliberately thin: it is a *root*, not a container. Director
//! does not store tasks inside a project object — tasks live in the substrate
//! and are referenced by id. The project exists to anchor ids, scopes, and
//! per-project defaults, and to answer the one question that matters when
//! multiple projects are open: "which working directory is this?"

use serde::{Deserialize, Serialize};

use crate::ids::ProjectId;

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
    /// When Director first registered it.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When it last changed.
    pub updated_at: chrono::DateTime<chrono::Utc>,
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
            created_at: now,
            updated_at: now,
        }
    }

    /// Record a change, stamping `updated_at`.
    pub fn touch(&mut self) {
        self.updated_at = chrono::Utc::now();
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
}
