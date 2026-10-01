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
//! The check it runs is the machine-checkable form a plan attached to the
//! task's [`ExpectedOutput`]s — a test command, a build, a script. Orqyn runs
//! it through an [`ExecutionProvider`] and reads the exit code itself. The
//! agent whose work is being judged never gets to report the outcome, and the
//! provider that runs the command never gets to interpret it. That separation
//! is the acceptance criterion "agent claims done, tests fail → must not
//! become completed", made mechanical.
//!
//! ## What VERIFY is responsible for, and what it is not
//!
//! VERIFY owns the *verdict*. Three are possible, and only two of them write:
//!
//! - **Pass** — every check ran and exited zero. The task becomes
//!   [`TaskStatus::Done`].
//! - **Fail** — a check ran and did not exit zero, or it hung past the
//!   executor's timeout. The task becomes [`TaskStatus::Failed`], and deciding
//!   what happens to failed work is REPLAN's job.
//! - **Unverifiable** — the round could not gather the evidence it needed, so
//!   it writes nothing and leaves the task awaiting verification. Either the
//!   task names no check at all ([`UnverifiableReason::NoChecks`]), or a check
//!   exists but the round could not run it
//!   ([`UnverifiableReason::EvidenceMissing`]) — the executor refused, or the
//!   command would not parse. An unrunnable check is *not* a failure: broken
//!   evidence is a problem with the environment, not with the work, and
//!   marking work failed because the harness could not run a test would make
//!   every environment hiccup a false verdict.
//!
//! A task whose criteria are partly prose — an [`ExpectedOutput`] with a
//! criterion but no `check` — is reported as [`CheckResult::Unchecked`], and it
//! never blocks a pass. Orqyn verifies what it can check; a criterion it has
//! no machine check for is reported to the caller, not silently satisfied.
//!
//! VERIFY does not decide what a failed task should do next, does not choose
//! who reworks it, and does not inspect git or files directly. The richer
//! evidence-gathering the README describes — git diff inspection, file
//! presence, test suite orchestration — is the verification engine's later
//! work; this step is the loop's judgment, and the checks it runs are the ones
//! the plan already named.
//!
//! ## Why the write is the task row
//!
//! The verdict is one [`TaskRepository::update_task`], which moves the status
//! and appends the transition to the history in the same transaction. There is
//! no second fact that has to land with it — MONITOR's report already ended the
//! tenure, closed the session, and freed the agent, so a task awaiting
//! verification has no tenant and no session left to clean up. That is why the
//! report ends the tenure when it does rather than when the verdict lands: it
//! keeps this step's write to a single row.

use std::path::Path;

use director_domain::ids::{ProjectId, TaskId};
use director_domain::providers::{CommandSpec, ExecutionProvider};
use director_domain::task::{Task, TaskStatus};
use director_domain::TaskRepository;
use director_store::Store;

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
/// reported. A task is done after this round only if a check actually passed.
///
/// The round is idempotent. A task it passed is `done`, so the next round does
/// not survey it; a task it failed is `failed` and likewise out of scope. A
/// task it could not judge stays `verification_pending` and is surveyed again
/// next round, which is safe precisely because the round wrote nothing about
/// it.
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
        let checks = run_checks(executor, task, working_dir).await;
        let verdict = judge(&checks);
        let outcome = verdict_destination(&verdict);

        // The verdict is one task write, status and history together. A task
        // the round could not judge is left exactly as it was.
        if let Some(status) = outcome {
            let mut updated = task.clone();
            updated.status = status;
            store.tasks().update_task(&updated).await?;
        }

        judged.push(Judged {
            task: task.id.clone(),
            verdict,
            checks,
            outcome,
        });
    }

    Ok(VerifyReport { judged })
}

/// The status a verdict moves a task to, or `None` when the verdict writes
/// nothing. Kept beside [`judge`] so the mapping from a verdict to a write is
/// visible in one place.
fn verdict_destination(verdict: &Verdict) -> Option<TaskStatus> {
    match verdict {
        Verdict::Passed => Some(TaskStatus::Done),
        Verdict::Failed => Some(TaskStatus::Failed),
        Verdict::Unverifiable(_) => None,
    }
}

/// Run every check a task names, in the task's order, and collect what
/// happened.
///
/// A criterion with no `check` is [`CheckResult::Unchecked`] — reported, never
/// blocking. A check the executor cannot run is [`CheckResult::Unrunnable`],
/// which is evidence missing rather than work failing. Everything else is the
/// outcome the executor observed.
async fn run_checks<E>(executor: &E, task: &Task, working_dir: &Path) -> Vec<CheckResult>
where
    E: ExecutionProvider,
{
    let mut results = Vec::with_capacity(task.expected_outputs.len());
    for output in &task.expected_outputs {
        // The check as written, trimmed: a blank one is no check at all, and
        // becomes a criterion Orqyn has no machine form for.
        let Some(check) = output
            .check
            .as_deref()
            .map(str::trim)
            .filter(|check| !check.is_empty())
        else {
            results.push(CheckResult::Unchecked {
                criterion: output.criterion.clone(),
            });
            continue;
        };

        let Some(spec) = parse_check(check, working_dir) else {
            // A check that is not a program and arguments cannot be run, and
            // guessing a program would be executing something the plan did not
            // name.
            results.push(CheckResult::Unrunnable {
                criterion: output.criterion.clone(),
                command: check.to_string(),
                error: "the check does not name a program to run".to_string(),
            });
            continue;
        };

        match executor.run_command(&spec).await {
            Ok(outcome) => {
                if outcome.succeeded() {
                    results.push(CheckResult::Passed {
                        criterion: output.criterion.clone(),
                        command: check.to_string(),
                    });
                } else {
                    results.push(CheckResult::Failed(Box::new(FailedCheck {
                        criterion: output.criterion.clone(),
                        command: check.to_string(),
                        exit_code: outcome.exit_code,
                        timed_out: outcome.timed_out,
                        stdout: outcome.stdout,
                        stderr: outcome.stderr,
                    })));
                }
            }
            // The executor could not run the command at all — no program by
            // that name, or the substrate refuses to execute. This is not a
            // verdict on the work; the round reports the task unverifiable
            // and the caller fixes the environment.
            Err(error) => {
                results.push(CheckResult::Unrunnable {
                    criterion: output.criterion.clone(),
                    command: check.to_string(),
                    error: error.to_string(),
                });
            }
        }
    }
    results
}

/// The pure half of verification: given what the round observed, what is the
/// verdict?
///
/// No store, no I/O, no executor — everything about the world has already been
/// gathered by [`run_checks`]. Extracted so the rules can be tested directly,
/// because the order of precedence between them is the whole design:
///
/// 1. A check that ran and failed decides. Nothing else the round saw undoes
///    an observed failure, so `Failed` comes first.
/// 2. A check the round could not run leaves the evidence incomplete, so the
///    task is `Unverifiable` — but only if nothing failed, because a task with
///    one failing check and one unrunnable one has been judged.
/// 3. Otherwise, at least one check must have passed. A task whose criteria
///    are all prose has nothing Orqyn verified, and Orqyn does not complete
///    work it did not check.
pub fn judge(checks: &[CheckResult]) -> Verdict {
    if checks
        .iter()
        .any(|check| matches!(check, CheckResult::Failed(_)))
    {
        return Verdict::Failed;
    }

    if checks
        .iter()
        .any(|check| matches!(check, CheckResult::Unrunnable { .. }))
    {
        return Verdict::Unverifiable(UnverifiableReason::EvidenceMissing);
    }

    if checks
        .iter()
        .any(|check| matches!(check, CheckResult::Passed { .. }))
    {
        return Verdict::Passed;
    }

    Verdict::Unverifiable(UnverifiableReason::NoChecks)
}

/// Turn an expected output's check into a command to run, in `working_dir`.
///
/// The check is one command line: program first, arguments after, split on
/// whitespace. There is no shell — no quoting, no globs, no substitution — and
/// that is deliberate at this stage. Anything an agent could hide inside a
/// shell expansion is something Orqyn would execute without looking at it, so
/// a check too elaborate for this shape is expected to name a script file the
/// caller wrote, not to become a shell expression.
///
/// Returns `None` for a check that names no program, which the round treats as
/// unrunnable rather than choosing a program itself.
fn parse_check(check: &str, working_dir: &Path) -> Option<CommandSpec> {
    let mut parts = check.split_whitespace();
    let program = parts.next()?.to_string();
    let args = parts.map(str::to_string).collect();
    Some(CommandSpec {
        program,
        args,
        working_dir: Some(working_dir.to_string_lossy().into_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn a_check_splits_into_program_and_arguments() {
        let spec =
            parse_check("  git   log --oneline  ", Path::new("/repo")).expect("a parseable check");
        assert_eq!(spec.program, "git");
        assert_eq!(spec.args, vec!["log", "--oneline"]);
        assert_eq!(spec.working_dir.as_deref(), Some("/repo"));
    }

    #[test]
    fn a_check_with_no_program_is_not_a_command() {
        assert!(parse_check("", Path::new("/repo")).is_none());
        assert!(parse_check("   ", Path::new("/repo")).is_none());
    }

    #[test]
    fn a_program_with_no_arguments_is_still_a_command() {
        let spec = parse_check("git", Path::new("/repo")).expect("a bare program");
        assert_eq!(spec.program, "git");
        assert!(spec.args.is_empty());
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
}
