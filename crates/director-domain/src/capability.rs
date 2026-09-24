//! Capabilities an [Agent](crate::agent::Agent) can declare and a
//! [Task](crate::task::Task) can require.
//!
//! Capabilities are the basis of assignment (Phase 8): Director matches a
//! task's `required_capabilities` against the capabilities an agent declares.
//! They are deliberately coarse. This is not a skills ontology; it is a filter
//! that prevents assigning "write the Postgres migration" to an agent with no
//! database capability.

use serde::{Deserialize, Serialize};

/// A named capability.
///
/// The fixed variants cover the common coding-agent roles. [`Capability::Custom`]
/// exists so a deployment can express domain-specific skills (e.g. an internal
/// framework) without forcing a release of Director. Custom capabilities are
/// matched case-insensitively by string, so `Custom("Payments".into())` and
/// `Custom("payments".into())` should be normalized before comparison —
/// [`Capability::matches`] does that normalization.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// General-purpose code changes in an unfamiliar area.
    Coding,
    /// Writing and running tests, including adversarial test design.
    Testing,
    /// Reviewing another agent's diff for correctness and style.
    Reviewing,
    /// Decomposing work into tasks and sequencing them.
    Planning,
    /// Authoring or updating documentation.
    Documentation,
    /// Investigation, prior-art reading, prototyping.
    Research,
    /// User-facing UI work.
    Frontend,
    /// Server/API work.
    Backend,
    /// Schema, migrations, query tuning.
    Database,
    /// CI, deployment, infrastructure.
    DevOps,
    /// Security review and threat modelling.
    Security,
    /// A human contributor. Distinct from every machine agent: humans do not
    /// heartbeat, and Director must never block on one.
    Human,
    /// A deployment-specific capability, matched by name.
    Custom(String),
}

impl Capability {
    /// True if `self` satisfies `required`.
    ///
    /// A concrete capability satisfies itself. `Coding` is treated as a
    /// generalist super-capability: an agent declaring `Coding` satisfies any
    /// non-human requirement, because a general coding agent can attempt any
    /// coding task. This is intentional — Director's assignment is a *filter*,
    /// not a guarantee of success; the verification engine (Phase 10) is what
    /// catches failure.
    ///
    /// `Human` is never satisfied by a machine capability and never required
    /// of one: it exists so a task can be flagged "needs a person".
    pub fn matches(&self, required: &Capability) -> bool {
        match (self, required) {
            // A task that needs a person is only satisfied by a person.
            (Capability::Human, Capability::Human) => true,
            (_, Capability::Human) => false,

            // A generalist can attempt any coding task.
            (Capability::Coding, _) => true,

            // Custom capabilities match on a normalized name. This arm must
            // come before the discriminant guard below: two `Custom` values
            // share a discriminant, so that guard would otherwise short-circuit
            // and make any two custom capabilities match each other.
            (Capability::Custom(have), Capability::Custom(need)) => {
                have.trim().eq_ignore_ascii_case(need.trim())
            }

            // Exact kind match.
            (a, b) if std::mem::discriminant(a) == std::mem::discriminant(b) => true,

            _ => false,
        }
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Capability::Custom(name) => write!(f, "custom:{name}"),
            other => write!(f, "{}", serde_json::to_string(other).unwrap_or_default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_capabilities_match() {
        assert!(Capability::Testing.matches(&Capability::Testing));
        assert!(Capability::Database.matches(&Capability::Database));
    }

    #[test]
    fn mismatched_capabilities_do_not_match() {
        assert!(!Capability::Frontend.matches(&Capability::Backend));
    }

    #[test]
    fn coding_is_a_generalist_super_capability() {
        assert!(Capability::Coding.matches(&Capability::Database));
        assert!(Capability::Coding.matches(&Capability::Security));
    }

    #[test]
    fn only_a_human_satisfies_a_human_requirement() {
        assert!(Capability::Human.matches(&Capability::Human));
        assert!(!Capability::Coding.matches(&Capability::Human));
        assert!(!Capability::Backend.matches(&Capability::Human));
    }

    #[test]
    fn custom_capabilities_match_case_insensitively() {
        assert!(
            Capability::Custom("Payments".into()).matches(&Capability::Custom("payments".into()))
        );
        assert!(
            !Capability::Custom("Payments".into()).matches(&Capability::Custom("Billing".into()))
        );
    }

    #[test]
    fn capabilities_round_trip_through_serde() {
        let caps = vec![
            Capability::Coding,
            Capability::Database,
            Capability::Custom("payments".into()),
        ];
        let json = serde_json::to_string(&caps).unwrap();
        let back: Vec<Capability> = serde_json::from_str(&json).unwrap();
        assert_eq!(caps, back);
    }
}
