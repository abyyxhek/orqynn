//! The VERIFY step of Orqyn's loop.
//!
//! VERIFY reads what MONITOR left behind: a task whose holder said it was
//! done. It judges that claim by *independent* evidence — commands Orqyn runs
//! itself, whose exit codes Orqyn reads itself — and it is the only step that
//! may move a task to [`TaskStatus::Done`].
//!
//! ```text
//! OBSERVE → PLAN → ASSIGN → MONITOR → VERIFY → REPLAN
//!                              ▲
//!                              │
//!                     verification_pending tasks
//!                     + the checks their
//!                       expected outputs name
//! ```
//!
//! ## Why this step exists
//!
//! The whole model rests on one rule: **nothing self-reports completion**. An
//! agent's "done" moves a task to [`TaskStatus::VerificationPending`] and no
//! further — that is what MONITOR's [`crate::monitor::report_done`] does, and
//! it is the whole reason the report stops where it does. VERIFY is the other
//! half of the bargain: it turns a claim into a verdict by looking at the work
//! rather than at the claim.
//!
//! ## What this step is now, and what it was
//!
//! Phase 6 landed this step as the judgment itself: it ran the checks, read the
//! exit codes, and wrote the verdict as one [`TaskRepository::update_task`].
//! That was correct about the judgment and silent about the reasoning — a task
//! that reached `done` through it carried no durable record of what was checked
//! or what the checks saw. Phase 10 moves the judgment into the
//! [verification engine](crate::engine) and leaves this step as the loop's
//! *report* on it:
//!
//! ```text
//! expected outputs ──▶ engine ──▶ probes ──▶ executor ──▶ evidence
//!                        │                                  │
//!                        ▼                                  ▼
//!                   judge(evidence) ──▶ Verification ──▶ Store::apply_verification
//!                        │                                  │
//!                        ▼                                  ▼
//!                    this step's report              task status + history
//! ```
//!
//! The engine owns the verdict and the write. It lands both through
//! [`Store::apply_verification`] in one transaction — the verification row, the
//! task's status move, and the transition history row that records the move —
//! which is what makes "a `done` task always has a verification behind it" a
//! property of the store rather than a habit of the caller. This step no longer
//! calls `update_task` at all; the [`Judged`] entries it returns are a read of
//! what the engine and the store decided.
//!
//! The checks it reports are still the machine-checkable form a plan attached to
//! the task's [`ExpectedOutput`]s — a test command, a build, a script. Orqyn
//! runs them through an [`ExecutionProvider`] and reads the exit code itself.
//! The agent whose work is being judged never gets to report the outcome, and
//! the provider that runs the command never gets to interpret it. That
//! separation is the acceptance criterion "agent claims done, tests fail → must
//! not become completed", made mechanical.
//!
//! ## What VERIFY is responsible for, and what it is not
//!
//! VERIFY reports three verdicts, and only two of them move anything:
//!
//! - **Pass** — every check ran and exited zero. The task becomes
//!   [`TaskStatus::Done`], and a [`Verification`] record says why.
//! - **Fail** — a check ran and did not exit zero, or it hung past the
//!   executor's timeout. The task becomes [`TaskStatus::Failed`], and deciding
//!   what happens to failed work is REPLAN's job.
//! - **Unverifiable** — the round could not gather the evidence it needed, so
//!   it moves nothing and leaves the task awaiting verification. Either the
//!   task names no check at all ([`UnverifiableReason::NoChecks`]), or a check
//!   exists but the round could not run it
//!   ([`UnverifiableReason::EvidenceMissing`]) — the executor refused, or the
//!   command would not parse. An unrunnable check is *not* a failure: broken
//!   evidence is a problem with the environment, not with the work, and marking
//!   work failed because the harness could not run a test would make every
//!   environment hiccup a false verdict. The record is still written — "Orqyn
//!   looked, and could not yet tell" is history worth keeping.
//!
//! A task whose criteria are partly prose — an [`ExpectedOutput`] with a
//! criterion but no `check` — is reported as [`CheckResult::Unchecked`], and it
//! never blocks a pass. Orqyn verifies what it can check; a criterion it has no
//! machine check for is reported to the caller, not silently satisfied.
//!
//! VERIFY does not decide what a failed task should do next, does not choose
//! who reworks it, and does not inspect git or files directly. The richer
//! evidence-gathering the README describes — git diff inspection, file
//! presence, test suite orchestration — is the engine's later work through the
//! [`Probe`](director_domain::verification::Probe) model; this step is the
//! loop's judgment, and the checks it names are the ones the plan already
//! named.
//!
//! [`Store::apply_verification`]: director_store::Store::apply_verification
//! [`Verification`]: director_domain::verification::Verification
//! [`ExpectedOutput`]: director_domain::task::ExpectedOutput

use std::path::Path;

use director_domain::ids::{ProjectId, TaskId};
use director_domain::providers::ExecutionProvider;
use director_domain::task::{Task, TaskStatus};
use director_domain::verification::{Evidence, EvidenceStatus, ProbeKind, VerificationStatus};
use director_domain::TaskRepository;
use director_store::Store;

use crate::engine::{self, ProbeOutcome};
use crate::VerifyError;

/// One round of VERIFY: every task awaiting a verdict, and what the round
/// concluded about it.
#[derive(Debug, Clone, Default)]
pub struct VerifyReport {
    /// Every task the round surveyed, in the store's order. A round that found
    /// nothing awaiting verification is an empty report, not an error.
    pub judged: Vec<Judged>,
}

impl VerifyReport {
    /// The ids of the tasks the round passed — now `done`.
    pub fn passed(&self) -> impl Iterator<Item = &TaskId> {
        self.judged
            .iter()
            .filter(|judged| matches!(judged.verdict, Verdict::Passed))
            .map(|judged| &judged.task)
    }

    /// The ids of the tasks the round failed — now `failed`, and REPLAN's
    /// business.
    pub fn failed(&self) -> impl Iterator<Item = &TaskId> {
        self.judged
            .iter()
            .filter(|judged| matches!(judged.verdict, Verdict::Failed))
            .map(|judged| &judged.task)
    }

    /// The ids of the tasks the round could not judge, left awaiting
    /// verification. A caller polling this wants to know: something claimed
    /// done and Orqyn still cannot say.
    pub fn unverifiable(&self) -> impl Iterator<Item = &TaskId> {
        self.judged
            .iter()
            .filter(|judged| matches!(judged.verdict, Verdict::Unverifiable(_)))
            .map(|judged| &judged.task)
    }

    /// True if every task the round surveyed passed. Empty is true for the same
    /// reason [`crate::monitor::MonitorReport::is_quiet`] is true on an empty
    /// round: nothing is awaiting a verdict, and nothing awaiting one failed.
    pub fn all_passed(&self) -> bool {
        self.judged
            .iter()
            .all(|judged| matches!(judged.verdict, Verdict::Passed))
    }
}

/// What the round concluded about one task, and the evidence it used.
#[derive(Debug, Clone)]
pub struct Judged {
    /// The task that was judged.
    pub task: TaskId,
    /// The verdict the round reached.
    pub verdict: Verdict,
    /// Every expected output, in the task's order, with what the round did
    /// about it. This is the evidence trail for the verdict, and it is what a
    /// caller shows a human asking "why did this pass or fail?".
    pub checks: Vec<CheckResult>,
    /// The status the round wrote, if it wrote one. `None` for a task the round
    /// left awaiting verification.
    pub outcome: Option<TaskStatus>,
}

/// The verdict one round reached about one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every check ran and passed. The task moved to `done`.
    Passed,
    /// A check ran and did not pass. The task moved to `failed`.
    Failed,
    /// The round could not reach a verdict and wrote nothing, for the reason
    /// given.
    Unverifiable(UnverifiableReason),
}

/// Why a round could not judge a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnverifiableReason {
    /// No expected output names a check, so there was nothing to run. A task
    /// Orqyn has no check for is a task Orqyn cannot complete — the planner
    /// has to give it a checkable criterion.
    NoChecks,
    /// A check exists but the round could not run it: the executor refused to
    /// run the command, or the command did not parse into a program and
    /// arguments. The evidence is missing, not the work.
    EvidenceMissing,
}

/// What the round did about one expected output.
#[derive(Debug, Clone)]
pub enum CheckResult {
    /// The check ran and exited zero.
    Passed {
        /// The criterion this check was for.
        criterion: String,
        /// The command the round ran.
        command: String,
    },
    /// The check ran and did not exit zero, or it exceeded the executor's
    /// timeout.
    ///
    /// Boxed because it carries the command's full output, which dwarfs every
    /// other arm — an unboxed enum would size each `Passed` report at the cost
    /// of a whole command's stdout.
    Failed(Box<FailedCheck>),
    /// The check could not be run at all. The error is carried so a caller can
    /// tell a broken environment from broken work.
    Unrunnable {
        /// The criterion this check was for.
        criterion: String,
        /// The check as written, which the round could not execute.
        command: String,
        /// Why the round could not run it, in the executor's own terms.
        error: String,
    },
    /// The criterion has no machine-checkable form. Reported, never blocking:
    /// Orqyn verifies what it can check and says plainly what it could not.
    Unchecked {
        /// The criterion Orqyn has no check for.
        criterion: String,
    },
}

/// A check that ran and did not pass, with everything it produced.
#[derive(Debug, Clone)]
pub struct FailedCheck {
    /// The criterion this check was for.
    pub criterion: String,
    /// The command the round ran.
    pub command: String,
    /// The process exit code; `-1` if unavailable.
    pub exit_code: i32,
    /// True if the executor killed the command for exceeding its limit. A hang
    /// is a failure worth reporting distinctly from a wrong answer.
    pub timed_out: bool,
    /// The command's standard output.
    pub stdout: String,
    /// The command's standard error.
    pub stderr: String,
}

/// Run the VERIFY step: judge every task awaiting verification by running the
/// checks its expected outputs name.
///
/// Each check runs in `working_dir` through `executor`, and the round reads
/// only the exit codes and output it observes — never anything the agent
/// reported. A task is done after this round only if a check actually passed,
/// and only through a [`Verification`] the engine persisted alongside the move.
///
/// The round is idempotent. A task it passed is `done`, so the next round does
/// not survey it; a task it failed is `failed` and likewise out of scope. A
/// task it could not judge stays `verification_pending` and is surveyed again
/// next round, which is safe precisely because the round wrote nothing about
/// it that would have to be unwritten — the engine still records the attempt as
/// history, and history is append-only.
pub async fn verify<E>(
    store: &Store,
    executor: &E,
    project_id: &ProjectId,
    working_dir: &Path,
) -> Result<VerifyReport, VerifyError>
where
    E: ExecutionProvider,
{
    let tasks = store.tasks().list_tasks(project_id).await?;

    let mut judged = Vec::new();
    for task in tasks
        .iter()
        .filter(|task| task.status == TaskStatus::VerificationPending)
    {
        // The engine owns the judgment and the write. It resolves this task's
        // checks into probes, gathers the evidence through the executor,
        // reaches a verdict, and lands it — record, status move, and history
        // row — in one transaction. This step reports what came back.
        let round =
            engine::verify_task(store, executor, task, working_dir, chrono::Utc::now()).await?;

        judged.push(Judged {
            task: task.id.clone(),
            verdict: verdict_from(
                &round.verification.status,
                round
                    .outcomes
                    .iter()
                    .any(|outcome| outcome.status == EvidenceStatus::Unverifiable),
            ),
            checks: checks_for(task, &round.outcomes),
            // The destination is derived from the verdict by the model itself,
            // so the report and the store cannot disagree about what a `Passed`
            // verdict means.
            outcome: round.verification.status.task_status(),
        });
    }

    Ok(VerifyReport { judged })
}

/// The [`Verdict`] the engine's status implies, with the reason an
/// unverifiable round gives a caller.
///
/// The model keeps [`VerificationStatus::Unverifiable`] as one status because
/// the reason moves nothing — it is for the caller's report, not the task's
/// state. The distinction is re-derived here, from the evidence the round
/// gathered: an empty evidence list means the plan named nothing checkable, and
/// a list holding an unrunnable check means Orqyn knew what to run and could
/// not run it.
fn verdict_from(status: &VerificationStatus, evidence_missing: bool) -> Verdict {
    match status {
        VerificationStatus::Passed => Verdict::Passed,
        VerificationStatus::Failed => Verdict::Failed,
        VerificationStatus::Unverifiable if evidence_missing => {
            Verdict::Unverifiable(UnverifiableReason::EvidenceMissing)
        }
        VerificationStatus::Unverifiable => Verdict::Unverifiable(UnverifiableReason::NoChecks),
    }
}

/// Every expected output with what the engine did about it, in the task's own
/// order.
///
/// [`engine::probes_for`] walks these same expected outputs in this same order
/// and keeps exactly the checkable ones, so the outcomes arrive as an ordered
/// subsequence of the task's criteria: the next outcome is this output's
/// whenever this output named a check, and a criterion with no check is
/// [`CheckResult::Unchecked`] — reported, never blocking. The round reports the
/// task's full contract, not only the parts Orqyn could run.
fn checks_for(task: &Task, outcomes: &[ProbeOutcome]) -> Vec<CheckResult> {
    let mut results = Vec::with_capacity(task.expected_outputs.len());
    let mut outcomes = outcomes.iter();
    for output in &task.expected_outputs {
        let check = match outcomes.next() {
            Some(outcome) if outcome.criterion == output.criterion => check_from(outcome),
            _ => CheckResult::Unchecked {
                criterion: output.criterion.clone(),
            },
        };
        results.push(check);
    }
    results
}

/// One engine outcome as the loop's report shape. [`CheckResult`] is what a
/// caller reads, and it carries the command's streams and exit code — the
/// detail too rich and too report-shaped to persist, which the caller already
/// has in the task it asked about.
fn check_from(outcome: &ProbeOutcome) -> CheckResult {
    let command = outcome.command.clone().unwrap_or_default();
    match outcome.status {
        EvidenceStatus::Passed => CheckResult::Passed {
            criterion: outcome.criterion.clone(),
            command,
        },
        EvidenceStatus::Failed => CheckResult::Failed(Box::new(FailedCheck {
            criterion: outcome.criterion.clone(),
            command,
            exit_code: outcome.exit_code,
            timed_out: outcome.timed_out,
            stdout: outcome.stdout.clone(),
            stderr: outcome.stderr.clone(),
        })),
        EvidenceStatus::Unverifiable => CheckResult::Unrunnable {
            criterion: outcome.criterion.clone(),
            command,
            error: if outcome.note.is_empty() {
                "the probe gathered no evidence".to_string()
            } else {
                outcome.note.clone()
            },
        },
        // Advisory evidence decides nothing, so it is reported as a criterion
        // Orqyn did not check rather than as a pass or a failure.
        EvidenceStatus::Observed => CheckResult::Unchecked {
            criterion: outcome.criterion.clone(),
        },
    }
}

/// The pure half of the step, kept for callers that already hold
/// [`CheckResult`]s: given what a round observed, what is the verdict?
///
/// Delegates to [`engine::judge`] so the precedence rules live in one place —
/// this is the loop's view of the same rules, over the loop's own report types
/// rather than the engine's evidence. Prose criteria contribute no evidence at
/// all, which is what keeps them from blocking a pass or manufacturing one.
pub fn judge(checks: &[CheckResult]) -> Verdict {
    let evidence: Vec<Evidence> = checks.iter().filter_map(evidence_for_check).collect();
    let evidence_missing = evidence
        .iter()
        .any(|evidence| evidence.status == EvidenceStatus::Unverifiable);
    verdict_from(&engine::judge(&evidence), evidence_missing)
}

/// The evidence one [`CheckResult`] contributes, if any. [`CheckResult::Unchecked`]
/// is prose and contributes nothing: Orqyn did not ask about it, so the record
/// does not pretend it did.
fn evidence_for_check(check: &CheckResult) -> Option<Evidence> {
    match check {
        CheckResult::Passed { criterion, command } => Some(Evidence {
            kind: ProbeKind::Command,
            criterion: criterion.clone(),
            status: EvidenceStatus::Passed,
            detail: format!("the check passed: {command}"),
        }),
        CheckResult::Failed(failed) => Some(Evidence {
            kind: ProbeKind::Command,
            criterion: failed.criterion.clone(),
            status: EvidenceStatus::Failed,
            detail: format!(
                "the check failed ({}) — exit {}, stderr: {}",
                failed.command, failed.exit_code, failed.stderr
            ),
        }),
        CheckResult::Unrunnable {
            criterion,
            command,
            error,
        } => Some(Evidence {
            kind: ProbeKind::Command,
            criterion: criterion.clone(),
            status: EvidenceStatus::Unverifiable,
            detail: format!("the check could not be run ({command}): {error}"),
        }),
        CheckResult::Unchecked { .. } => None,
    }
}

/// The status a verdict moves a task to, or `None` when the verdict writes
/// nothing. The mapping lives on the model —
/// [`VerificationStatus::task_status`] — and is surfaced here so a caller
/// reading this step sees the same destination the store wrote.
pub fn verdict_destination(verdict: &Verdict) -> Option<TaskStatus> {
    match verdict {
        Verdict::Passed => Some(TaskStatus::Done),
        Verdict::Failed => Some(TaskStatus::Failed),
        Verdict::Unverifiable(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use director_domain::ids::TaskId;
    use director_domain::task::ExpectedOutput;

    /// An expected output with a machine check.
    fn checked(criterion: &str, check: &str) -> ExpectedOutput {
        ExpectedOutput {
            criterion: criterion.to_string(),
            check: Some(check.to_string()),
        }
    }

    /// A criterion the planner wrote as prose only.
    fn prose(criterion: &str) -> ExpectedOutput {
        ExpectedOutput {
            criterion: criterion.to_string(),
            check: None,
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

    #[test]
    fn checks_that_all_pass_pass_the_task() {
        let task = task_with(&[
            checked("login works", "git --version"),
            checked("logout works", "git --version"),
        ]);
        let results = vec![
            CheckResult::Passed {
                criterion: task.expected_outputs[0].criterion.clone(),
                command: "git --version".into(),
            },
            CheckResult::Passed {
                criterion: task.expected_outputs[1].criterion.clone(),
                command: "git --version".into(),
            },
        ];

        assert_eq!(judge(&results), Verdict::Passed);
        assert_eq!(
            verdict_destination(&Verdict::Passed),
            Some(TaskStatus::Done)
        );
    }

    #[test]
    fn a_failed_check_fails_the_task_even_when_others_pass() {
        // The point of verification: one observed failure outweighs any number
        // of passes and any amount of unchecked prose.
        let results = vec![
            CheckResult::Passed {
                criterion: "login works".into(),
                command: "git --version".into(),
            },
            CheckResult::Failed(Box::new(FailedCheck {
                criterion: "logout works".into(),
                command: "git --nope".into(),
                exit_code: 129,
                timed_out: false,
                stdout: String::new(),
                stderr: "unknown option".into(),
            })),
            CheckResult::Unchecked {
                criterion: "the prose looks right".into(),
            },
        ];

        assert_eq!(judge(&results), Verdict::Failed);
        assert_eq!(
            verdict_destination(&Verdict::Failed),
            Some(TaskStatus::Failed)
        );
    }

    #[test]
    fn a_hung_check_is_a_failure() {
        // A timeout is an observed outcome, not missing evidence.
        let results = vec![CheckResult::Failed(Box::new(FailedCheck {
            criterion: "the suite terminates".into(),
            command: "cargo test".into(),
            exit_code: -1,
            timed_out: true,
            stdout: String::new(),
            stderr: String::new(),
        }))];

        assert_eq!(judge(&results), Verdict::Failed);
    }

    #[test]
    fn an_unrunnable_check_leaves_the_task_unverifiable() {
        // The evidence could not be gathered, so nothing is written. This is
        // not a failure of the work.
        let results = vec![
            CheckResult::Passed {
                criterion: "login works".into(),
                command: "git --version".into(),
            },
            CheckResult::Unrunnable {
                criterion: "logout works".into(),
                command: "definitely-not-a-program".into(),
                error: "no such program".into(),
            },
        ];

        assert_eq!(
            judge(&results),
            Verdict::Unverifiable(UnverifiableReason::EvidenceMissing)
        );
        assert_eq!(
            verdict_destination(&Verdict::Unverifiable(UnverifiableReason::EvidenceMissing)),
            None,
            "an unverifiable task is left as it was"
        );
    }

    #[test]
    fn a_task_with_only_prose_criteria_cannot_be_verified() {
        // Orqyn completes what it can check. Criteria with no machine form are
        // reported and never satisfied.
        let results = vec![
            CheckResult::Unchecked {
                criterion: "it feels right".into(),
            },
            CheckResult::Unchecked {
                criterion: "the docs read well".into(),
            },
        ];

        assert_eq!(
            judge(&results),
            Verdict::Unverifiable(UnverifiableReason::NoChecks)
        );
    }

    #[test]
    fn a_task_with_no_expected_outputs_at_all_cannot_be_verified() {
        assert_eq!(
            judge(&[]),
            Verdict::Unverifiable(UnverifiableReason::NoChecks)
        );
    }

    #[test]
    fn prose_criteria_do_not_block_a_pass() {
        // A task with one real check that passes and two unchecked criteria is
        // done: Orqyn verified what it could and reported what it could not.
        let results = vec![
            CheckResult::Passed {
                criterion: "login works".into(),
                command: "git --version".into(),
            },
            CheckResult::Unchecked {
                criterion: "it feels right".into(),
            },
        ];

        assert_eq!(judge(&results), Verdict::Passed);
    }

    #[test]
    fn the_report_renders_prose_where_the_engine_ran_nothing() {
        // `checks_for` walks the task's whole contract, so a criterion with no
        // check is reported as unchecked even when the outcomes hold a pass for
        // the criterion before it.
        let task = task_with(&[
            checked("login works", "git --version"),
            prose("the error messages read well"),
        ]);
        let outcomes = vec![ProbeOutcome {
            kind: ProbeKind::Command,
            criterion: "login works".into(),
            command: Some("git --version".into()),
            status: EvidenceStatus::Passed,
            exit_code: 0,
            timed_out: false,
            stdout: "git version".into(),
            stderr: String::new(),
            note: String::new(),
        }];

        let checks = checks_for(&task, &outcomes);
        assert_eq!(checks.len(), 2);
        assert!(matches!(
            checks[0],
            CheckResult::Passed { ref criterion, .. } if criterion == "login works"
        ));
        assert!(matches!(
            checks[1],
            CheckResult::Unchecked { ref criterion } if criterion == "the error messages read well"
        ));
    }

    #[test]
    fn the_report_carries_what_a_failed_command_printed() {
        // The detail that makes a failure actionable survives the mapping from
        // the engine's outcome to the loop's report.
        let task = task_with(&[checked("the endpoint answers", "git --nope")]);
        let outcomes = vec![ProbeOutcome {
            kind: ProbeKind::Command,
            criterion: "the endpoint answers".into(),
            command: Some("git --nope".into()),
            status: EvidenceStatus::Failed,
            exit_code: 129,
            timed_out: false,
            stdout: String::new(),
            stderr: "unknown option".into(),
            note: String::new(),
        }];

        let checks = checks_for(&task, &outcomes);
        let failed = match &checks[0] {
            CheckResult::Failed(failed) => failed,
            other => panic!("expected a failed check, got {other:?}"),
        };
        assert_eq!(failed.command, "git --nope");
        assert_eq!(failed.exit_code, 129);
        assert_eq!(failed.stderr, "unknown option");

        assert_eq!(
            verdict_from(&VerificationStatus::Failed, false),
            Verdict::Failed
        );
    }

    #[test]
    fn an_unverifiable_round_reports_why_in_terms_a_caller_can_act_on() {
        // A round that knew what to run and could not run it is evidence
        // missing; a round with nothing to run at all is no checks. The two
        // are reported differently because a caller fixes different things.
        let unrunnable = ProbeOutcome {
            kind: ProbeKind::Command,
            criterion: "the endpoint answers".into(),
            command: Some("nope".into()),
            status: EvidenceStatus::Unverifiable,
            exit_code: 0,
            timed_out: false,
            stdout: String::new(),
            stderr: String::new(),
            note: "spawn failed".into(),
        };
        // The evidence-missing signal is read off the outcomes the engine
        // gathered, the same way `verify` reads it for the round's report.
        let evidence_missing = [unrunnable]
            .iter()
            .any(|outcome| outcome.status == EvidenceStatus::Unverifiable);
        assert!(evidence_missing);
        assert_eq!(
            verdict_from(&VerificationStatus::Unverifiable, evidence_missing),
            Verdict::Unverifiable(UnverifiableReason::EvidenceMissing)
        );
        assert_eq!(
            verdict_from(&VerificationStatus::Unverifiable, false),
            Verdict::Unverifiable(UnverifiableReason::NoChecks)
        );
    }

    #[test]
    fn an_empty_report_has_passed_everything_it_surveyed() {
        // The same convention as a quiet MONITOR round: nothing is awaiting a
        // verdict, and nothing awaiting one failed.
        let report = VerifyReport::default();
        assert!(report.all_passed());
        assert_eq!(report.passed().count(), 0);
        assert_eq!(report.failed().count(), 0);
        assert_eq!(report.unverifiable().count(), 0);

        let mixed = VerifyReport {
            judged: vec![
                Judged {
                    task: TaskId::from_string("A"),
                    verdict: Verdict::Passed,
                    checks: vec![],
                    outcome: Some(TaskStatus::Done),
                },
                Judged {
                    task: TaskId::from_string("B"),
                    verdict: Verdict::Failed,
                    checks: vec![],
                    outcome: Some(TaskStatus::Failed),
                },
                Judged {
                    task: TaskId::from_string("C"),
                    verdict: Verdict::Unverifiable(UnverifiableReason::NoChecks),
                    checks: vec![],
                    outcome: None,
                },
            ],
        };
        assert!(!mixed.all_passed());
        assert_eq!(
            mixed.passed().map(|id| id.as_str()).collect::<Vec<_>>(),
            vec!["A"]
        );
        assert_eq!(
            mixed.failed().map(|id| id.as_str()).collect::<Vec<_>>(),
            vec!["B"]
        );
        assert_eq!(
            mixed
                .unverifiable()
                .map(|id| id.as_str())
                .collect::<Vec<_>>(),
            vec!["C"]
        );
    }

    /// The engine is the only writer of a verdict, so this step never has to
    /// translate a status the model does not already know how to map.
    #[test]
    fn the_steps_verdict_mapping_agrees_with_the_model() {
        assert_eq!(
            engine::destination(VerificationStatus::Passed),
            verdict_destination(&Verdict::Passed)
        );
        assert_eq!(
            engine::destination(VerificationStatus::Failed),
            verdict_destination(&Verdict::Failed)
        );
        assert_eq!(
            engine::destination(VerificationStatus::Unverifiable),
            verdict_destination(&Verdict::Unverifiable(UnverifiableReason::NoChecks))
        );
    }

    /// An engine round arrives with the same destination the model derives, so
    /// a caller reading `Judged::outcome` sees what the store actually wrote.
    #[test]
    fn an_engine_rounds_outcome_is_the_models_mapping() {
        let round = crate::engine::EngineRound {
            verification: director_domain::verification::Verification::new(
                director_domain::ids::VerificationId::from_string("VER-1"),
                TaskId::from_string("A"),
                ProjectId::from_string("PROJ-1"),
                None,
                VerificationStatus::Passed,
                vec![],
                None,
                chrono::Utc::now(),
            ),
            outcomes: vec![],
        };
        assert_eq!(
            round.verification.status.task_status(),
            Some(TaskStatus::Done)
        );
    }
}
