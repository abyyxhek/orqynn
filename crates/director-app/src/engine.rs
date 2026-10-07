//! The verification engine — the thing that gathers evidence and lands a
//! verdict (Phase 10).
//!
//! ## Why this module exists
//!
//! The loop's VERIFY step, as Phase 6 landed it, runs the machine-checkable
//! commands a plan named and reads their exit codes. That is correct, and it is
//! what holds the model's central invariant: nothing self-reports completion.
//! What it did *not* do was remember. A task that reached
//! [`TaskStatus::Done`] through it carried no durable record of which checks
//! ran, what they saw, or why the verdict was what it was — the judgment was as
//! ephemeral as the round that made it.
//!
//! The record layer ([`director_domain::verification`]) fixed half of that: a
//! [`Verification`] is a verdict persisted, with its evidence trail. What had no
//! caller was the *engine* — the code that resolves a task's checkable
//! requirements into probes, gathers the evidence, applies the precedence rules,
//! builds that record, and writes it through
//! [`Store::apply_verification`]. This module is that engine, and the loop's
//! VERIFY step ([`crate::verify`]) is now its only caller.
//!
//! ## What the engine is and is not
//!
//! It is **independent of the agent that did the work**. It takes no agent id,
//! reads no agent report, and consults no session: it reads what the executor
//! and the store's own recorded project state say, and only those. The agent
//! being judged is never asked whether its own work is finished, and the
//! provider running a command never interprets the command for the caller. An
//! agent's report is what puts a task in front of the engine; it is never the
//! verdict the engine reaches.
//!
//! It is **not a second verification system**. Phase 6's `verify` ran command
//! checks; the engine runs the same commands, through the same
//! [`ExecutionProvider`], and reports them as [`Evidence`]. The [`Probe`] model
//! exists so the other questions Orqyn can ask — a test suite, a diff, a path
//! check — have a place to land when they are implemented. This module starts
//! with [`Probe::Command`], because that is the question the loop already asks;
//! the other three kinds are reached through [`run_probe`], which is the single
//! extension point.
//!
//! ## How the engine moves a task
//!
//! It does not. [`verify_task`] never calls `update_task`; the verdict and the
//! task's status move are one transaction inside
//! [`Store::apply_verification`], which is what makes "a `done` task always has
//! a verification behind it" a property of the store rather than a habit of the
//! caller. The engine's contribution to that guarantee is narrower and just as
//! load-bearing: it hands the store the `state_version` of the task it read
//! before it gathered its evidence, so a task that moved under the round is
//! refused rather than re-judged on evidence that no longer describes it.
//!
//! ## The precedence rules
//!
//! [`judge`] is the pure half of the engine, lifted out of Phase 6's `verify`
//! because the order of precedence between the evidence statuses *is* the
//! design, and it is worth testing without running anything:
//!
//! 1. **`Failed` decides first.** One observed failure outweighs any amount of
//!    passing or missing evidence, so a task with one failing check is
//!    [`VerificationStatus::Failed`] no matter what else the round saw.
//! 2. **`Unverifiable` is second.** Evidence the round could not gather means
//!    the verdict is not reachable — but only when nothing failed, because a
//!    task with one failing check and one unrunnable one *has* been judged.
//!    `Unverifiable` is a property of the environment, never a verdict on the
//!    work, and it is what makes the task safe to survey again next tick.
//! 3. **`Passed` is third.** At least one decisive pass is required; a task
//!    whose criteria are all prose has nothing Orqyn verified, and Orqyn does
//!    not complete work it did not check.
//! 4. **`Observed` never decides.** Advisory evidence is reported in the record
//!    and then deliberately ignored — a diff is circumstantial, so it can inform
//!    a human without satisfying a criterion.
//!
//! [`Store::apply_verification`]: director_store::Store::apply_verification
//! [`TaskStatus::Done`]: director_domain::task::TaskStatus::Done
//! [`Verification`]: director_domain::verification::Verification
//! [`Evidence`]: director_domain::verification::Evidence
//! [`Probe`]: director_domain::verification::Probe
//! [`Probe::Command`]: director_domain::verification::Probe::Command
//! [`ExecutionProvider`]: director_domain::providers::ExecutionProvider

use std::path::Path;

use director_domain::ids::{ProjectId, RepositoryId, TaskId, VerificationId};
use director_domain::providers::{CommandSpec, ExecutionProvider};
use director_domain::task::Task;
use director_domain::verification::{
    Evidence, EvidenceStatus, Probe, ProbeKind, Verification, VerificationStatus,
};
use director_domain::{Id, ProjectStateRepository, VerificationRepository};
use director_store::Store;

/// Run the engine for one task: resolve the probes its plan names, gather their
/// evidence, reach a verdict, and land it through the store as a durable
/// [`Verification`].
///
/// This is the only path by which the engine records a judgment. It never calls
/// `update_task` and never sets a status itself: the verdict and the task's move
/// are one transaction inside [`Store::apply_verification`], along with the
/// transition history row that records the move.
///
/// `working_dir` is where command probes run — the same directory Phase 6's
/// `verify` ran them in. `task.state_version` is the optimistic-concurrency
/// guard: it is the version the caller read, so a task that moved between the
/// read and the landing is refused with
/// [`VerifyTaskError::Store`] holding a [`StateVersionConflict`] rather than
/// re-judged. Nothing about the round is lost by that refusal, because nothing
/// was written: the verification row and the status move commit together or not
/// at all.
///
/// Returns the verification as the store now holds it, read back rather than
/// echoed from the input, alongside the per-probe outcomes a report wants.
///
/// [`Store::apply_verification`]: director_store::Store::apply_verification
/// [`StateVersionConflict`]: director_domain::StoreError::StateVersionConflict
pub async fn verify_task<E>(
    store: &Store,
    executor: &E,
    task: &Task,
    working_dir: &Path,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<EngineRound, VerifyTaskError>
where
    E: ExecutionProvider,
{
    let Some(project_id) = task.project_id.clone() else {
        return Err(VerifyTaskError::NoProject(task.id.clone()));
    };

    // The probes the task's expected outputs translate into. Phase 6 ran these
    // as checks; the engine runs the same commands as `Probe::Command`, so the
    // evidence lands in the record instead of evaporating with the round.
    let probes = probes_for(task);

    let mut outcomes = Vec::with_capacity(probes.len());
    for probe in &probes {
        outcomes.push(run_probe(probe, executor, working_dir).await);
    }

    // The evidence is derived once: the verdict is reached on it and the record
    // persists it, and deriving it twice would be two chances to disagree.
    let evidence = outcomes
        .iter()
        .map(ProbeOutcome::to_evidence)
        .collect::<Vec<_>>();
    let status = judge(&evidence);
    let (repository_id, head_commit) = observed_state(store, &project_id).await?;

    let verification = Verification::new(
        next_verification_id(store, &task.id).await?,
        task.id.clone(),
        project_id,
        repository_id,
        status,
        evidence,
        head_commit,
        now,
    );

    // The verdict lands atomically with the task's move and the history row.
    // The version the engine presents is the one the caller read, because that
    // is the task the evidence describes.
    let (stored, _) = store
        .apply_verification(&verification, task.state_version, now)
        .await?;

    Ok(EngineRound {
        verification: stored,
        outcomes,
    })
}

/// What one engine round produced about one task: the durable record, plus the
/// per-probe detail a caller's report needs.
///
/// The split is deliberate. The [`Verification`] is what persists, and its
/// evidence is prose that a human reads. The [`ProbeOutcome`]s carry the
/// structured detail a report renders — the exit code, the streams, the command
/// as named — which is too rich and too report-shaped to be a stored column, and
/// which the caller already has in the task it asked about.
#[derive(Debug, Clone)]
pub struct EngineRound {
    /// The verification as the store now holds it, evidence included.
    pub verification: Verification,
    /// What each probe observed, in the order the probes were asked.
    pub outcomes: Vec<ProbeOutcome>,
}

/// The [`Probe`]s a task's expected outputs translate into, in the task's own
/// order.
///
/// An expected output with no checkable form yields no probe — it is prose, and
/// the engine does not invent a question for it. A task with no probes at all is
/// one Orqyn has no machine question for, and [`judge`] says so as
/// [`VerificationStatus::Unverifiable`] rather than guessing. Keeping this the
/// engine's job is what makes "which checks does this task name" answerable one
/// way, for the record layer and the loop's report alike.
pub fn probes_for(task: &Task) -> Vec<Probe> {
    task.expected_outputs
        .iter()
        .filter_map(|output| {
            let check = output
                .check
                .as_deref()
                .map(str::trim)
                .filter(|check| !check.is_empty())?;
            Some(Probe::Command {
                criterion: output.criterion.clone(),
                command: check.to_string(),
            })
        })
        .collect()
}

/// Gather the evidence for one probe, whichever kind it is.
///
/// This is the engine's single extension point. `Probe::Command` is implemented
/// against the [`ExecutionProvider`] the loop already uses; the other three
/// kinds have no implementation yet, and a probe the engine cannot gather is
/// recorded as [`EvidenceStatus::Unverifiable`] — the honest answer, "Orqyn
/// looked and could not yet tell", rather than fabricated evidence. Adding a
/// probe kind later is adding an arm here, and nothing else.
async fn run_probe<E>(probe: &Probe, executor: &E, working_dir: &Path) -> ProbeOutcome
where
    E: ExecutionProvider,
{
    match probe {
        Probe::Command { criterion, command } => {
            run_command_probe(criterion, command, executor, working_dir).await
        }
        // Not yet implemented — deliberately. The record layer models these so
        // the engine has somewhere to put them, and `Unverifiable` here is what
        // keeps an unimplemented probe from being mistaken for a passed or
        // failed one. A caller naming one gets history recording that Orqyn
        // could not gather that evidence yet.
        Probe::TestSuite { criterion, .. }
        | Probe::Diff { criterion, .. }
        | Probe::File { criterion, .. } => ProbeOutcome {
            kind: probe.kind(),
            criterion: criterion.clone(),
            command: None,
            status: EvidenceStatus::Unverifiable,
            exit_code: 0,
            timed_out: false,
            stdout: String::new(),
            stderr: String::new(),
            note: "this probe kind is not implemented yet".to_string(),
        },
    }
}

/// Run one command probe and report what the executor actually saw.
///
/// The engine never interprets the command for the caller and never asks the
/// agent anything: it hands the command to the executor and reads the exit code
/// itself. Three outcomes, and only the first two decide anything:
///
/// - The command ran and exited clean → [`EvidenceStatus::Passed`].
/// - The command ran and did not → [`EvidenceStatus::Failed`], with everything
///   it printed, because that is what a human needs to see why the work failed.
///   A timeout lands here too: a hung suite is an observed outcome, not missing
///   evidence.
/// - The executor could not run it at all → [`EvidenceStatus::Unverifiable`].
///   This is the environment, not the work, and it is the distinction that keeps
///   a broken harness from marking good work failed.
async fn run_command_probe<E>(
    criterion: &str,
    command: &str,
    executor: &E,
    working_dir: &Path,
) -> ProbeOutcome
where
    E: ExecutionProvider,
{
    let Some(spec) = parse_command(command, working_dir) else {
        return ProbeOutcome {
            kind: ProbeKind::Command,
            criterion: criterion.to_string(),
            command: Some(command.to_string()),
            status: EvidenceStatus::Unverifiable,
            exit_code: 0,
            timed_out: false,
            stdout: String::new(),
            stderr: String::new(),
            note: "the check does not name a program to run".to_string(),
        };
    };

    match executor.run_command(&spec).await {
        Ok(outcome) => {
            let status = if outcome.succeeded() {
                EvidenceStatus::Passed
            } else {
                EvidenceStatus::Failed
            };
            ProbeOutcome {
                kind: ProbeKind::Command,
                criterion: criterion.to_string(),
                command: Some(command.to_string()),
                status,
                exit_code: outcome.exit_code,
                timed_out: outcome.timed_out,
                stdout: outcome.stdout,
                stderr: outcome.stderr,
                note: String::new(),
            }
        }
        // The executor refused — no such program, or the substrate cannot
        // execute. Evidence is missing, and that is not a verdict on the work.
        Err(error) => ProbeOutcome {
            kind: ProbeKind::Command,
            criterion: criterion.to_string(),
            command: Some(command.to_string()),
            status: EvidenceStatus::Unverifiable,
            exit_code: 0,
            timed_out: false,
            stdout: String::new(),
            stderr: String::new(),
            note: error.to_string(),
        },
    }
}

/// The pure half of the engine: given the evidence a round gathered, what is the
/// verdict?
///
/// No store, no I/O, no executor — everything about the world has already been
/// gathered. The precedence between the statuses is the design, and it is the
/// same precedence Phase 6's `verify` applied to its checks, now expressed once
/// on the evidence model so the engine and any future caller cannot disagree:
///
/// 1. A decisive failure decides, immediately and regardless of anything else.
/// 2. Missing evidence blocks the verdict, unless a failure already decided it.
/// 3. Otherwise a decisive pass is required; advisory evidence never satisfies
///    this, because `Observed` decides nothing by construction.
/// 4. Nothing decisive at all means the round could not judge the work.
pub fn judge(evidence: &[Evidence]) -> VerificationStatus {
    if evidence
        .iter()
        .any(|evidence| evidence.status.is_decisive_failure())
    {
        return VerificationStatus::Failed;
    }

    if evidence
        .iter()
        .any(|evidence| evidence.status == EvidenceStatus::Unverifiable)
    {
        return VerificationStatus::Unverifiable;
    }

    if evidence
        .iter()
        .any(|evidence| evidence.status.is_decisive_pass())
    {
        return VerificationStatus::Passed;
    }

    VerificationStatus::Unverifiable
}

/// What one probe observed, in the detail a caller's report renders.
///
/// This is the engine's working type: rich enough to show a human exactly what
/// happened (the command, the exit code, both streams, whether it timed out),
/// and the source the persisted [`Evidence`] is derived from. It is deliberately
/// not the stored shape — the store keeps the verdict and prose a reader needs,
/// not a process's stdout — and it carries no more authority than the probe that
/// produced it.
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    /// The kind of probe that gathered this.
    pub kind: ProbeKind,
    /// The criterion this evidence speaks to.
    pub criterion: String,
    /// The command the probe ran, when it ran one. `None` for a probe kind the
    /// engine does not yet execute.
    pub command: Option<String>,
    /// Whether the evidence supports the claim, contradicts it, or says nothing.
    pub status: EvidenceStatus,
    /// The process exit code; `0` when no process ran.
    pub exit_code: i32,
    /// True if the executor killed the command for exceeding its limit. A hang
    /// is an observed failure, not missing evidence.
    pub timed_out: bool,
    /// The command's standard output.
    pub stdout: String,
    /// The command's standard error.
    pub stderr: String,
    /// Why the probe could gather no evidence, when it could not. Empty for a
    /// probe that ran, because the outcome speaks for itself.
    pub note: String,
}

impl ProbeOutcome {
    /// The persisted shape of this outcome: a status the engine reasons over,
    /// and prose a human reads. Everything structural stays in the report.
    pub fn to_evidence(&self) -> Evidence {
        let detail = match (&self.command, self.status) {
            // A command that could not run never produced an exit code, so the
            // reason is the whole story.
            (Some(command), EvidenceStatus::Unverifiable) => {
                format!("the command could not be run ({command}): {}", self.note)
            }
            (Some(command), _) => format_command_outcome(command, self),
            (None, _) => self.note.clone(),
        };
        Evidence {
            kind: self.kind,
            criterion: self.criterion.clone(),
            status: self.status,
            detail,
        }
    }
}

/// Render a command's outcome as the `detail` a human reads in the record. This
/// is what makes a stored failure actionable: the judgment says *failed*, and
/// the evidence says what the command actually printed.
fn format_command_outcome(command: &str, outcome: &ProbeOutcome) -> String {
    let exit = if outcome.timed_out {
        "timed out".to_string()
    } else {
        outcome.exit_code.to_string()
    };
    format!(
        "command: {command}\nexit: {exit}\nstdout:\n{}\nstderr:\n{}",
        outcome.stdout, outcome.stderr
    )
}

/// The status a verdict moves a task to — the model's own mapping, exposed here
/// so a caller driving the engine can report the outcome without re-deriving it.
/// [`VerificationStatus::task_status`] is the single source of this mapping; the
/// store and the engine cannot disagree about what `Passed` means.
pub fn destination(status: VerificationStatus) -> Option<director_domain::task::TaskStatus> {
    status.task_status()
}

/// The repository and `HEAD` commit the evidence was gathered against, read from
/// the state Orqyn itself recorded for the project.
///
/// A verdict is a statement about a specific state of the repository, and this
/// is what pins it. The source is [`ProjectStateRepository`] rather than a fresh
/// git observation on purpose: the engine judges the work against the state
/// Orqyn already believes the project is in, not against whatever git happens to
/// hold mid-round, and a project Orqyn has never observed has no state to pin a
/// verdict to — `None` for both, honestly, rather than a invented commit.
async fn observed_state(
    store: &Store,
    project_id: &ProjectId,
) -> Result<(Option<RepositoryId>, Option<String>), VerifyTaskError> {
    let Some(state) = store.project_state().get_project_state(project_id).await? else {
        return Ok((None, None));
    };
    Ok((state.repository_id, Some(state.head_commit)))
}

/// The id the next verification of this task will carry.
///
/// A verification is append-only history, so the id has to be new every time
/// without being random. It follows the same shape
/// [`IdGenerator`](director_domain::ids::IdGenerator) mints everywhere else —
/// `<PREFIX>-<key>-<n>`, here `VER-<task>-<n>` — where `n` counts the judgments
/// already on record for this task. The task id is globally unique, so the
/// combination is too, which is what keeps a re-judgment from colliding with
/// the row it is deliberately not replacing. Reading the count from the store
/// rather than a per-process counter is what makes the id right after a restart
/// as well as within one tick.
async fn next_verification_id(
    store: &Store,
    task: &TaskId,
) -> Result<VerificationId, VerifyTaskError> {
    let already = store
        .verifications()
        .verifications_for_task(task)
        .await?
        .len();
    Ok(VerificationId::from_string(format!(
        "{}-{task}-{}",
        VerificationId::PREFIX,
        already + 1
    )))
}

/// Turn a check string into a command to run, in `working_dir`.
///
/// The check is one command line: program first, arguments after, split on
/// whitespace. There is no shell — no quoting, no globs, no substitution — and
/// that is deliberate. Anything an agent could hide inside a shell expansion is
/// something Orqyn would execute without looking at it, so a check too elaborate
/// for this shape is expected to name a script file the caller wrote, not to
/// become a shell expression.
///
/// Returns `None` for a check that names no program, which the engine treats as
/// unrunnable rather than choosing a program itself.
fn parse_command(check: &str, working_dir: &Path) -> Option<CommandSpec> {
    let mut parts = check.split_whitespace();
    let program = parts.next()?.to_string();
    let args = parts.map(str::to_string).collect();
    Some(CommandSpec {
        program,
        args,
        working_dir: Some(working_dir.to_string_lossy().into_owned()),
    })
}

/// Every way an engine round can fail.
#[derive(Debug, thiserror::Error)]
pub enum VerifyTaskError {
    /// The task belongs to no project, so there is nowhere to persist a
    /// judgment about it.
    #[error("the task {0} belongs to no project, so it cannot be verified")]
    NoProject(TaskId),
    /// The store rejected the verdict — most likely because the task moved
    /// between the round reading it and landing the judgment, in which case the
    /// evidence no longer describes the work and the caller re-reads and
    /// re-runs.
    #[error("the store rejected the verification: {0}")]
    Store(#[from] director_domain::StoreError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use director_adapters::executor::LocalExecutor;
    use director_domain::task::ExpectedOutput;

    /// An expected output with a machine check.
    fn checked(criterion: &str, check: &str) -> ExpectedOutput {
        ExpectedOutput {
            criterion: criterion.to_string(),
            check: Some(check.to_string()),
        }
    }

    /// A task whose expected outputs are the given checks.
    fn task_with(outputs: &[ExpectedOutput]) -> Task {
        let mut task = Task::for_project(
            ProjectId::from_string("PROJ-1"),
            TaskId::from_string("A"),
            "A",
            "do it",
        );
        task.expected_outputs = outputs.to_vec();
        task
    }

    /// Evidence of the given status, for one criterion.
    fn evidence(status: EvidenceStatus) -> Evidence {
        Evidence {
            kind: ProbeKind::Command,
            criterion: "c".to_string(),
            status,
            detail: "d".to_string(),
        }
    }

    #[test]
    fn a_failed_check_decides_over_everything_else() {
        // The precedence rule that matters most: one observed failure outweighs
        // any amount of passing or missing evidence.
        assert_eq!(
            judge(&[
                evidence(EvidenceStatus::Passed),
                evidence(EvidenceStatus::Failed),
                evidence(EvidenceStatus::Unverifiable),
            ]),
            VerificationStatus::Failed
        );
    }

    #[test]
    fn missing_evidence_blocks_a_verdict_unless_something_failed() {
        assert_eq!(
            judge(&[
                evidence(EvidenceStatus::Passed),
                evidence(EvidenceStatus::Unverifiable)
            ]),
            VerificationStatus::Unverifiable
        );
        // ...but a task that also failed has been judged.
        assert_eq!(
            judge(&[
                evidence(EvidenceStatus::Unverifiable),
                evidence(EvidenceStatus::Failed)
            ]),
            VerificationStatus::Failed
        );
    }

    #[test]
    fn a_pass_requires_decisive_evidence() {
        assert_eq!(
            judge(&[evidence(EvidenceStatus::Passed)]),
            VerificationStatus::Passed
        );
    }

    #[test]
    fn observed_evidence_decides_nothing() {
        // Advisory evidence alone never passes a task, and never fails it.
        assert_eq!(
            judge(&[evidence(EvidenceStatus::Observed)]),
            VerificationStatus::Unverifiable
        );
        // It does not block a pass either — it is reported and then ignored.
        assert_eq!(
            judge(&[
                evidence(EvidenceStatus::Passed),
                evidence(EvidenceStatus::Observed)
            ]),
            VerificationStatus::Passed
        );
    }

    #[test]
    fn no_evidence_is_unverifiable_not_passed() {
        // A task whose criteria are all prose: Orqyn checked nothing, so it
        // completes nothing.
        assert_eq!(judge(&[]), VerificationStatus::Unverifiable);
    }

    #[test]
    fn unverifiable_and_failed_map_to_different_task_states() {
        // Unverifiable writes nothing; Failed writes `failed`. They are never
        // the same verdict, and this is the mapping that keeps them apart.
        assert_eq!(destination(VerificationStatus::Unverifiable), None);
        assert_eq!(
            destination(VerificationStatus::Failed),
            Some(director_domain::task::TaskStatus::Failed)
        );
        assert_eq!(
            destination(VerificationStatus::Passed),
            Some(director_domain::task::TaskStatus::Done)
        );
    }

    #[test]
    fn a_task_with_only_prose_expected_outputs_yields_no_probes() {
        let task = task_with(&[ExpectedOutput {
            criterion: "it works".into(),
            check: None,
        }]);
        assert!(probes_for(&task).is_empty());
    }

    #[test]
    fn a_blank_check_yields_no_probe() {
        // Whitespace is not a check, and a criterion whose check is blank is
        // prose as far as the engine is concerned.
        let task = task_with(&[checked("first", "git --version"), checked("second", "   ")]);
        let probes = probes_for(&task);
        assert_eq!(probes.len(), 1);
        assert_eq!(probes[0].criterion(), "first");
    }

    #[test]
    fn a_task_with_checks_yields_command_probes_in_order() {
        let task = task_with(&[
            checked("first", "git --version"),
            checked("second", "cargo --version"),
        ]);
        let probes = probes_for(&task);
        assert_eq!(probes.len(), 2);
        assert_eq!(probes[0].criterion(), "first");
        assert_eq!(probes[1].criterion(), "second");
        assert_eq!(probes[0].kind(), ProbeKind::Command);
    }

    #[test]
    fn a_check_splits_into_program_and_arguments() {
        let spec = parse_command("  git   log --oneline  ", Path::new("/repo"))
            .expect("a parseable check");
        assert_eq!(spec.program, "git");
        assert_eq!(spec.args, vec!["log", "--oneline"]);
        assert_eq!(spec.working_dir.as_deref(), Some("/repo"));
    }

    #[test]
    fn a_check_with_no_program_is_not_a_command() {
        assert!(parse_command("", Path::new("/repo")).is_none());
        assert!(parse_command("   ", Path::new("/repo")).is_none());
    }

    #[test]
    fn a_program_with_no_arguments_is_still_a_command() {
        let spec = parse_command("git", Path::new("/repo")).expect("a bare program");
        assert_eq!(spec.program, "git");
        assert!(spec.args.is_empty());
    }

    /// An executor that refuses everything, used where the outcome is not the
    /// point of the test. This is deliberately a distinct kind of failure from
    /// a command that runs and exits non-zero: the engine reports the first as
    /// `Unverifiable` and the second as `Failed`.
    struct RefusingExecutor;

    impl director_domain::providers::Provider for RefusingExecutor {
        type Error = director_domain::providers::ProviderError;
    }

    #[async_trait::async_trait]
    impl ExecutionProvider for RefusingExecutor {
        async fn run_command(
            &self,
            _command: &CommandSpec,
        ) -> Result<
            director_domain::providers::CommandOutcome,
            director_domain::providers::ProviderError,
        > {
            Err(director_domain::providers::ProviderError::Unsupported(
                "no executor".into(),
            ))
        }
    }

    #[tokio::test]
    async fn unimplemented_probe_kinds_are_unverifiable_not_evidence() {
        // The extension-point contract: a probe the engine cannot gather is
        // recorded honestly, never fabricated as passed or failed.
        let suite = Probe::TestSuite {
            criterion: "the suite passes".into(),
            command: "cargo test".into(),
        };
        let outcome = run_probe(&suite, &RefusingExecutor, Path::new(".")).await;
        assert_eq!(outcome.status, EvidenceStatus::Unverifiable);
        assert_eq!(outcome.kind, ProbeKind::TestSuite);
        assert_eq!(
            outcome.to_evidence().status,
            EvidenceStatus::Unverifiable,
            "the record carries the honest status"
        );
    }

    #[tokio::test]
    async fn a_command_the_executor_refuses_is_unverifiable() {
        // Not a failure of the work: the environment could not gather the
        // evidence, and the record says so rather than judging the task.
        let probe = Probe::Command {
            criterion: "the endpoint answers".into(),
            command: "definitely-not-a-program --ok".into(),
        };
        let outcome = run_probe(&probe, &RefusingExecutor, Path::new(".")).await;
        assert_eq!(outcome.status, EvidenceStatus::Unverifiable);
        assert!(!outcome.note.is_empty(), "the reason is carried");
        assert_eq!(outcome.to_evidence().status, EvidenceStatus::Unverifiable);
    }

    #[tokio::test]
    async fn a_command_that_runs_and_succeeds_is_passed() {
        // A real command through the reference executor: the evidence is what
        // the machine said, not what anything claimed.
        let probe = Probe::Command {
            criterion: "git is installed".into(),
            command: "git --version".into(),
        };
        let outcome = run_probe(&probe, &LocalExecutor::default(), Path::new(".")).await;
        assert_eq!(outcome.status, EvidenceStatus::Passed);
        assert_eq!(outcome.exit_code, 0);
        assert!(!outcome.stdout.is_empty(), "git said something");
        assert!(outcome.note.is_empty());
        let recorded = outcome.to_evidence();
        assert_eq!(recorded.status, EvidenceStatus::Passed);
        assert!(recorded.detail.contains("git --version"));
        assert!(recorded.detail.contains("exit: 0"));
    }

    #[tokio::test]
    async fn a_command_that_runs_and_fails_is_failed() {
        // The acceptance criterion, mechanically: the command really ran and
        // really exited non-zero, so the evidence contradicts the claim.
        let probe = Probe::Command {
            criterion: "git accepts the flag".into(),
            command: "git --no-such-flag".into(),
        };
        let outcome = run_probe(&probe, &LocalExecutor::default(), Path::new(".")).await;
        assert_eq!(outcome.status, EvidenceStatus::Failed);
        assert_ne!(outcome.exit_code, 0);
        assert!(!outcome.stderr.is_empty(), "git said why it failed");
        let recorded = outcome.to_evidence();
        assert_eq!(recorded.status, EvidenceStatus::Failed);
        assert!(recorded.detail.contains("git --no-such-flag"));
        assert!(recorded.detail.contains(&outcome.stderr));
    }
}
