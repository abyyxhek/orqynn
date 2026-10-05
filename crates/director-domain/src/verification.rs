//! The [Verification] — Orqyn's own record of judging a task's work (Phase 10).
//!
//! ## Why this module exists
//!
//! The whole model rests on one rule: **nothing self-reports completion**. An
//! agent's "done" moves a task to [`crate::task::TaskStatus::VerificationPending`]
//! and no further. Something has to turn that claim into a verdict by looking at
//! the work, and that something is Orqyn.
//!
//! Phase 6 landed the loop's VERIFY step, and it proved the rule is enforceable:
//! it runs the machine-checkable commands a plan named and reads their exit
//! codes itself. What it does *not* do is gather evidence the plan did not
//! name. That is this module's job. A [Probe] is a question Orqyn asks about the
//! work; [Evidence] is what the answer looked like; a [Verification] is the whole
//! act, persisted, so "why did this task reach `done`?" stays answerable after
//! the process that decided it is gone.
//!
//! ## The probes
//!
//! Four kinds, and each one is grounded in a capability that actually exists
//! rather than in an aspiration:
//!
//! - [`Probe::Command`] runs a command through an
//!   [`ExecutionProvider`](crate::providers::ExecutionProvider) and reads the
//!   exit code. This is what Phase 6's VERIFY step already did; the engine
//!   subsumes it rather than replacing it.
//! - [`Probe::TestSuite`] runs the project's test suite and parses how many
//!   tests passed. The command arrives from the caller, exactly as PLAN's task
//!   specs and REPLAN's remediations do — *which* suite is a reasoning step.
//! - [`Probe::Diff`] inspects what git says changed, against the task's declared
//!   [`scope`](crate::task::Task::scope_paths). This is corroborating evidence,
//!   deliberately non-decisive: a diff is circumstantial, and the decisive
//!   evidence is the suite. See the note on [EvidenceStatus::Observed].
//! - [`Probe::File`] asserts a path in the working tree exists or does not. The
//!   caller names the path for the same reason it names the suite.
//!
//! ## What a verification never does
//!
//! It never trusts an agent's report for the outcome, and it never interprets a
//! command's output for the caller — the engine reads exit codes and parses
//! counts, and anything requiring judgment is reported as evidence for a human
//! or a later reasoning step, never silently satisfied.

use serde::{Deserialize, Serialize};

use crate::ids::{ProjectId, RepositoryId, TaskId, VerificationId};

/// One question Orqyn asks about a task's work, and the shape of the answer.
///
/// A probe is data, not behaviour: the engine decides how to gather each kind,
/// and keeping the question serializable is what makes a [Verification] a
/// durable record of what was asked rather than a log line about what happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Probe {
    /// Run a command and read its exit code. The command is one line: program
    /// first, arguments after, split on whitespace — no shell, so nothing an
    /// agent could hide inside an expansion is executed unseen.
    Command {
        /// The criterion this probe speaks to.
        criterion: String,
        /// The command, as the plan named it.
        command: String,
    },
    /// Run the project's test suite and read how many tests passed.
    TestSuite {
        /// The criterion this probe speaks to.
        criterion: String,
        /// The command that runs the suite, named by the caller.
        command: String,
    },
    /// Inspect the git diff for evidence the task's declared scope changed.
    Diff {
        /// The criterion this probe speaks to.
        criterion: String,
        /// The paths the task declared it might touch. Empty means "anything
        /// anywhere", which is much weaker evidence and is reported as such.
        scope: Vec<String>,
    },
    /// Assert a path in the working tree exists, or does not.
    File {
        /// The criterion this probe speaks to.
        criterion: String,
        /// The path, repository-relative.
        path: String,
        /// Whether the path is expected to be present.
        should_exist: bool,
    },
}

impl Probe {
    /// The criterion this probe speaks to — the acceptance criterion whose
    /// satisfaction or violation the evidence describes.
    pub fn criterion(&self) -> &str {
        match self {
            Probe::Command { criterion, .. }
            | Probe::TestSuite { criterion, .. }
            | Probe::Diff { criterion, .. }
            | Probe::File { criterion, .. } => criterion,
        }
    }

    /// Which kind of probe this is. Used where a caller needs to group or count
    /// evidence by how it was gathered rather than by what it concluded.
    pub fn kind(&self) -> ProbeKind {
        match self {
            Probe::Command { .. } => ProbeKind::Command,
            Probe::TestSuite { .. } => ProbeKind::TestSuite,
            Probe::Diff { .. } => ProbeKind::Diff,
            Probe::File { .. } => ProbeKind::File,
        }
    }
}

/// The kind of probe, as a tag a report or a store column can carry without the
/// probe's full payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeKind {
    /// A command run through an execution provider.
    Command,
    /// The project's test suite.
    TestSuite,
    /// The git diff.
    Diff,
    /// A path in the working tree.
    File,
}

impl std::fmt::Display for ProbeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeKind::Command => f.write_str("command"),
            ProbeKind::TestSuite => f.write_str("test_suite"),
            ProbeKind::Diff => f.write_str("diff"),
            ProbeKind::File => f.write_str("file"),
        }
    }
}

/// What one [Probe] found, and whether it decides anything.
///
/// The status carries the verdict semantics; `detail` carries the diagnostics a
/// human needs to see why. The separation matters because the status is what the
/// engine reasons over and what gets persisted as a column, while `detail` is
/// prose that exists to be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// The kind of probe that gathered this.
    pub kind: ProbeKind,
    /// The criterion this evidence speaks to.
    pub criterion: String,
    /// Whether the evidence supports the claim, contradicts it, or says nothing.
    pub status: EvidenceStatus,
    /// What the probe saw, in a form a human can act on: the command and its
    /// output, the paths that changed, the suite's counts.
    pub detail: String,
}

/// Whether a piece of evidence decides a claim, and how.
///
/// The four values are not symmetric, and the asymmetry is the design:
///
/// - [`Passed`] and [`Failed`] decide. The engine only reaches them from a
///   machine signal it read itself — an exit code, a parsed count, a path check
///   against the filesystem.
/// - [`Unverifiable`] means the evidence could not be gathered: the executor
///   refused, the command would not parse, the repository could not be read.
///   **This is not a failure of the work.** Broken evidence is a property of the
///   environment, and marking work failed because the harness could not run a
///   test would make every environment hiccup a false verdict.
/// - [`Observed`] means the evidence was gathered but deliberately decides
///   nothing. A diff that touches nothing in the task's scope is suspicious; it
///   is not proof the work did not happen, because a diff is circumstantial —
///   the work may predate the observation window, or live in a path the task did
///   not declare. So the engine reports it, in detail, and lets the decisive
///   probes decide. `Observed` never blocks a pass and never causes a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    /// The probe ran and the evidence supports the claim.
    Passed,
    /// The probe ran and the evidence contradicts the claim.
    Failed,
    /// The probe could not gather evidence. The environment, not the work.
    Unverifiable,
    /// The probe gathered evidence that decides nothing. Reported, never
    /// decisive — see the type's own docs for why a diff is circumstantial.
    Observed,
}

impl EvidenceStatus {
    /// True if this evidence can be the reason a task reaches `done`.
    pub fn is_decisive_pass(self) -> bool {
        matches!(self, EvidenceStatus::Passed)
    }

    /// True if this evidence is a reason a task cannot reach `done` as-is.
    pub fn is_decisive_failure(self) -> bool {
        matches!(self, EvidenceStatus::Failed)
    }

    /// True if the engine should treat this evidence as absent when reasoning to
    /// a verdict — observed evidence is reported and then ignored.
    pub fn is_advisory(self) -> bool {
        matches!(self, EvidenceStatus::Observed)
    }
}

/// The outcome of one [Verification], as a stored fact.
///
/// This is the tri-state the loop's VERIFY step already reasoned over, now part
/// of the model so a stored [Verification] carries its own conclusion rather
/// than requiring a reader to re-derive it from the evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    /// Every decisive probe passed. The task may reach
    /// [`TaskStatus::Done`](crate::task::TaskStatus::Done).
    Passed,
    /// A decisive probe failed. The task needs rework, which is REPLAN's
    /// business.
    Failed,
    /// The round could not gather the evidence it needed, and wrote no
    /// conclusion. The task stays awaiting verification.
    Unverifiable,
}

impl VerificationStatus {
    /// The task status this verdict moves a task to, or `None` when the verdict
    /// writes nothing — the same mapping the loop's VERIFY step applies, now
    /// expressed on the model so the store and the step cannot disagree.
    pub fn task_status(self) -> Option<crate::task::TaskStatus> {
        match self {
            VerificationStatus::Passed => Some(crate::task::TaskStatus::Done),
            VerificationStatus::Failed => Some(crate::task::TaskStatus::Failed),
            VerificationStatus::Unverifiable => None,
        }
    }
}

/// One act of Orqyn judging a task's work: the probes it asked, the evidence it
/// gathered, and the verdict it reached, persisted.
///
/// A verification is append-only history, not a mutable record. Re-verifying a
/// task writes a *new* [Verification] with its own id, so "what did Orqyn
/// believe about TASK-42 on Tuesday, and what changed its mind on Wednesday" is
/// an ordinary query rather than a forensic exercise. The task that failed
/// verification twice and passed on the third attempt carries all three rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verification {
    /// This verification's identifier.
    pub id: VerificationId,
    /// The task whose work was judged.
    pub task_id: TaskId,
    /// The project the task belongs to.
    pub project_id: ProjectId,
    /// The repository the evidence was gathered from, when Orqyn has one
    /// registered for the project. Diff probes are meaningless without it, and
    /// recording it is what makes the evidence reproducible.
    pub repository_id: Option<RepositoryId>,
    /// The verdict the round reached.
    pub status: VerificationStatus,
    /// What each probe found, in the order the probes were asked. This is the
    /// evidence trail; a caller asking "why did this pass?" reads it here.
    pub evidence: Vec<Evidence>,
    /// The commit `HEAD` pointed at when the evidence was gathered, when a
    /// repository was available. A verdict is a statement about a specific state
    /// of the repository, and this is what pins it.
    pub head_commit: Option<String>,
    /// Monotonic version for optimistic concurrency — see
    /// [`crate::project::Project::state_version`]. A verification is append-only,
    /// so this advances only when a row is corrected rather than re-judged.
    #[serde(default = "default_state_version")]
    pub state_version: u64,
    /// When Orqyn gathered the evidence.
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// The version a freshly persisted record starts on. One, not zero: a record
/// that exists has been written once — the same baseline every mutable entity in
/// the model uses.
fn default_state_version() -> u64 {
    1
}

impl Verification {
    /// Build a verification from everything the engine gathered, deriving the
    /// verdict it should record. The engine supplies the verdict it reached
    /// rather than recomputing it here, because the precedence rules belong to
    /// the step that runs the probes; this constructor is the seam that keeps
    /// the record honest about what was asked.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: VerificationId,
        task_id: TaskId,
        project_id: ProjectId,
        repository_id: Option<RepositoryId>,
        status: VerificationStatus,
        evidence: Vec<Evidence>,
        head_commit: Option<String>,
        created_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Verification {
            id,
            task_id,
            project_id,
            repository_id,
            status,
            evidence,
            head_commit,
            state_version: default_state_version(),
            created_at,
        }
    }

    /// The criteria this verification gathered decisive evidence for — the ones
    /// a caller shows a human asking "what did Orqyn actually check?".
    pub fn decisive_criteria(&self) -> impl Iterator<Item = &Evidence> {
        self.evidence.iter().filter(|e| !e.status.is_advisory())
    }

    /// True if every decisive probe passed. Equivalent to
    /// [`VerificationStatus::Passed`], expressed on the record so a reader does
    /// not have to re-derive the verdict from the evidence to confirm it.
    pub fn is_passing(&self) -> bool {
        self.status == VerificationStatus::Passed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A probe reports the criterion it speaks to, whichever kind it is.
    #[test]
    fn every_probe_reports_its_criterion() {
        let command = Probe::Command {
            criterion: "login returns 200".into(),
            command: "curl -sf localhost:8080/login".into(),
        };
        let suite = Probe::TestSuite {
            criterion: "the suite passes".into(),
            command: "cargo test".into(),
        };
        let diff = Probe::Diff {
            criterion: "the work touched the declared scope".into(),
            scope: vec!["src/auth/".into()],
        };
        let file = Probe::File {
            criterion: "the module exists".into(),
            path: "src/auth/mod.rs".into(),
            should_exist: true,
        };

        for probe in [&command, &suite, &diff, &file] {
            assert!(!probe.criterion().is_empty());
        }
        assert_eq!(command.kind(), ProbeKind::Command);
        assert_eq!(suite.kind(), ProbeKind::TestSuite);
        assert_eq!(diff.kind(), ProbeKind::Diff);
        assert_eq!(file.kind(), ProbeKind::File);
    }

    /// The kinds render as stable, readable tags — these strings are what a
    /// report groups by and what a store column carries.
    #[test]
    fn probe_kinds_have_stable_tags() {
        assert_eq!(ProbeKind::Command.to_string(), "command");
        assert_eq!(ProbeKind::TestSuite.to_string(), "test_suite");
        assert_eq!(ProbeKind::Diff.to_string(), "diff");
        assert_eq!(ProbeKind::File.to_string(), "file");
    }

    /// Only Passed and Failed decide anything; Observed is advisory and
    /// Unverifiable is missing evidence rather than a verdict on the work.
    #[test]
    fn only_pass_and_fail_are_decisive() {
        assert!(EvidenceStatus::Passed.is_decisive_pass());
        assert!(!EvidenceStatus::Failed.is_decisive_pass());
        assert!(!EvidenceStatus::Unverifiable.is_decisive_pass());
        assert!(!EvidenceStatus::Observed.is_decisive_pass());

        assert!(EvidenceStatus::Failed.is_decisive_failure());
        assert!(!EvidenceStatus::Passed.is_decisive_failure());
        assert!(!EvidenceStatus::Unverifiable.is_decisive_failure());
        assert!(!EvidenceStatus::Observed.is_decisive_failure());

        assert!(EvidenceStatus::Observed.is_advisory());
        assert!(!EvidenceStatus::Passed.is_advisory());
    }

    /// The verdict maps onto a task status exactly the way the loop's VERIFY
    /// step maps it, and Unverifiable writes nothing.
    #[test]
    fn a_verdict_maps_to_one_task_status_or_none() {
        assert_eq!(
            VerificationStatus::Passed.task_status(),
            Some(crate::task::TaskStatus::Done)
        );
        assert_eq!(
            VerificationStatus::Failed.task_status(),
            Some(crate::task::TaskStatus::Failed)
        );
        assert_eq!(VerificationStatus::Unverifiable.task_status(), None);
    }

    /// A round-trip through serde is the persistence boundary: a verification
    /// that cannot survive serialization cannot be stored, and one that comes
    /// back different has silently lost evidence.
    #[test]
    fn a_verification_round_trips_through_serde() {
        let verification = Verification::new(
            VerificationId::from_string("VER-1"),
            TaskId::from_string("AUTH-42"),
            ProjectId::from_string("PROJ-1"),
            Some(RepositoryId::from_string("REPO-1")),
            VerificationStatus::Failed,
            vec![
                Evidence {
                    kind: ProbeKind::TestSuite,
                    criterion: "the suite passes".into(),
                    status: EvidenceStatus::Failed,
                    detail: "3 failed, 11 passed".into(),
                },
                Evidence {
                    kind: ProbeKind::Diff,
                    criterion: "the work touched the declared scope".into(),
                    status: EvidenceStatus::Observed,
                    detail: "no changes within src/auth/".into(),
                },
            ],
            Some("abc123".into()),
            chrono::Utc::now(),
        );

        let json = serde_json::to_string(&verification).expect("serializes");
        let back: Verification = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(verification, back);
    }

    /// A probe round-trips too, and the round trip preserves the kind — a
    /// `command` probe that came back as something else would be a probe the
    /// engine no longer knows how to gather.
    #[test]
    fn probes_round_trip_with_their_kind() {
        for probe in [
            Probe::Command {
                criterion: "c".into(),
                command: "git --version".into(),
            },
            Probe::TestSuite {
                criterion: "c".into(),
                command: "cargo test".into(),
            },
            Probe::Diff {
                criterion: "c".into(),
                scope: vec!["src/".into()],
            },
            Probe::File {
                criterion: "c".into(),
                path: "src/lib.rs".into(),
                should_exist: false,
            },
        ] {
            let json = serde_json::to_string(&probe).expect("serializes");
            let back: Probe = serde_json::from_str(&json).expect("deserializes");
            assert_eq!(probe, back);
        }
    }

    /// A record that reports its verdict can be trusted to be consistent with
    /// the evidence a reader can see on the same record.
    #[test]
    fn a_passing_record_reports_itself_as_passing() {
        let passing = Verification::new(
            VerificationId::from_string("VER-1"),
            TaskId::from_string("A"),
            ProjectId::from_string("PROJ-1"),
            None,
            VerificationStatus::Passed,
            vec![Evidence {
                kind: ProbeKind::TestSuite,
                criterion: "suite".into(),
                status: EvidenceStatus::Passed,
                detail: "14 passed".into(),
            }],
            None,
            chrono::Utc::now(),
        );
        assert!(passing.is_passing());
        assert_eq!(passing.decisive_criteria().count(), 1);

        let failing = Verification::new(
            VerificationId::from_string("VER-2"),
            TaskId::from_string("A"),
            ProjectId::from_string("PROJ-1"),
            None,
            VerificationStatus::Failed,
            vec![
                Evidence {
                    kind: ProbeKind::TestSuite,
                    criterion: "suite".into(),
                    status: EvidenceStatus::Failed,
                    detail: "1 failed".into(),
                },
                Evidence {
                    kind: ProbeKind::Diff,
                    criterion: "scope".into(),
                    status: EvidenceStatus::Observed,
                    detail: "nothing in scope".into(),
                },
            ],
            None,
            chrono::Utc::now(),
        );
        assert!(!failing.is_passing());
        // Advisory evidence is reported but not counted as decisive.
        assert_eq!(failing.decisive_criteria().count(), 1);
    }

    /// A fresh verification starts at version one — the same baseline every
    /// mutable entity in the model uses, so a reader comparing versions never
    /// has to special-case this one.
    #[test]
    fn a_new_verification_starts_at_version_one() {
        let verification = Verification::new(
            VerificationId::from_string("VER-1"),
            TaskId::from_string("A"),
            ProjectId::from_string("PROJ-1"),
            None,
            VerificationStatus::Unverifiable,
            vec![],
            None,
            chrono::Utc::now(),
        );
        assert_eq!(verification.state_version, 1);
    }
}
