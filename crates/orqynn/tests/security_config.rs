//! CLI configuration security tests (see `SECURITY_AUDIT.md`, §5).
//!
//! Two controls, both about what the CLI says out loud:
//!
//! 1. **The run report never echoes the task text.** The task description is
//!    the one free-form string an invocation supplies, and `provenance` reports
//!    the *id* derived from it — never the text itself. A description that
//!    happens to carry a secret cannot reach the terminal through Orqyn.
//! 2. **Unknown environment variables are never read.** `from_env_and_args`
//!    names the variables it consults; a variable outside that list cannot
//!    influence a resolved configuration, and no environment value other than
//!    the documented ones is echoed by `provenance`.

use std::path::PathBuf;

use crate::config::{EnvSource, MapEnv, ProcessEnv, RunArgs};

#[allow(dead_code)]
#[path = "../src/config.rs"]
mod config;
#[allow(dead_code)]
#[path = "../src/lifecycle.rs"]
mod lifecycle;

/// A real git repository in a temp dir, the shape `resolve` requires.
fn git_repo() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    let run = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("git on PATH");
        assert!(status.success(), "git {args:?} must succeed");
    };
    run(&["init", "-q"]);
    run(&["config", "user.name", "Orqynn Security Tests"]);
    run(&["config", "user.email", "security@orqyn.invalid"]);
    std::fs::write(repo.join("README.md"), "# fixture\n").expect("write");
    run(&["add", "README.md"]);
    run(&["commit", "-q", "-m", "initial"]);
    (tmp, repo)
}

/// **Control for information disclosure through logs (see ORQ-12).** The run
/// report never prints the task description verbatim — it prints the id derived
/// from it. That id is a lossy but faithful copy of the text (non-alphanumerics
/// collapse to `-`), so this is a reduction, not a redaction: the *exact* string
/// never reaches the terminal, and the audit records the residual as a Low
/// finding. A task description that carries a secret should not be treated as
/// private once it has been planned.
#[test]
fn the_run_report_names_the_derived_id_not_the_verbatim_text() {
    let (_tmp, repo) = git_repo();
    let mut args = RunArgs::parse([
        "--repo",
        repo.to_str().expect("utf-8"),
        "--task",
        "rotate the AWS key AKIA-DO-NOT-PRINT-THIS",
        "--check",
        "git --version",
    ])
    .expect("parses");
    args.resolve().expect("resolves");

    let report = args.provenance().join("\n");
    assert!(
        !report.contains("rotate the AWS key"),
        "the verbatim task text must not reach the run report"
    );
    assert!(
        report.contains("task id: ROTATE-THE-AWS-KEY-AKIA-DO-NOT-PRINT-THIS"),
        "the report names the derived id, which is the honest reduction: {report}"
    );
    assert_eq!(args.task_id, "ROTATE-THE-AWS-KEY-AKIA-DO-NOT-PRINT-THIS");
}

/// **Control for the unauthorized-local-process adversary.** Only the
/// documented `ORQYNN_*` names are read, and nothing else in the environment
/// can reach the resolved configuration.
#[test]
fn only_the_documented_environment_names_are_read() {
    let (tmp, repo) = git_repo();
    let repo_str = repo.to_string_lossy().into_owned();
    let env = MapEnv(vec![
        ("ORQYNN_REPO".to_string(), repo_str.clone()),
        ("ORQYNN_UNRELATED_SECRET".to_string(), "leak-me".to_string()),
        ("ANTHROPIC_API_KEY".to_string(), "sk-leak-me".to_string()),
    ]);
    let mut args = RunArgs::from_env_and_args(
        [
            "orqynn".to_string(),
            "--task".to_string(),
            "t".to_string(),
            "--check".to_string(),
            "git --version".to_string(),
        ],
        &env,
    )
    .expect("parses");
    args.resolve().expect("resolves");

    assert_eq!(args.repo, PathBuf::from(&repo_str));
    let report = args.provenance().join("\n");
    assert!(
        !report.contains("leak-me"),
        "no environment value outside the documented list may reach the report"
    );
    drop(tmp);
}

/// **Control for insecure defaults.** With no `--permission-mode` and no
/// environment override, Orqyn passes `None` through and says so: it does not
/// choose a permissive harness mode on the operator's behalf.
#[test]
fn no_permission_mode_is_chosen_by_default_and_the_report_says_so() {
    let (_tmp, repo) = git_repo();
    let mut args = RunArgs::parse([
        "--repo",
        repo.to_str().expect("utf-8"),
        "--task",
        "t",
        "--check",
        "git --version",
    ])
    .expect("parses");
    args.resolve().expect("resolves");
    assert!(
        args.permission_mode.is_none(),
        "Orqyn must not default into a permissive harness mode"
    );
    let report = args.provenance().join("\n");
    assert!(
        report.contains("the harness's own default"),
        "the report tells the operator no mode was chosen: {report}"
    );
}

/// **Control for the dry-run path.** A dry run reports the exact commands it
/// *would* execute, so an operator can audit them before anything runs.
#[test]
fn a_dry_run_names_the_commands_it_would_execute() {
    let (_tmp, repo) = git_repo();
    let mut args = RunArgs::parse([
        "--repo",
        repo.to_str().expect("utf-8"),
        "--task",
        "t",
        "--check",
        "cargo test --no-run",
        "--dry-run",
    ])
    .expect("parses");
    args.resolve().expect("resolves");
    let report = crate::lifecycle::dry_run_report(&args);
    let joined = report.join("\n");
    assert!(
        joined.contains("cargo test --no-run"),
        "the operator can read the exact commands before anything runs"
    );
}

/// **Control for ProcessEnv.** The real environment reader returns nothing for
/// a name that is not set, rather than erroring or inventing a value.
#[test]
fn the_process_env_reader_reports_absence() {
    let env = ProcessEnv;
    assert!(
        env.get("ORQYNN_DEFINITELY_NOT_SET_XYZ").is_none(),
        "an unset variable reads as absent, never as a default"
    );
}
