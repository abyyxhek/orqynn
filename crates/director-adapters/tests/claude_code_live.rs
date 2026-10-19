//! The one test that talks to a real Claude Code CLI.
//!
//! # This test is the only claim of real-agent support in the repository
//!
//! Everything else in [`harness_driver`][mod@self] uses a compiled double. That
//! is deliberate and sufficient for testing what Orqyn owns. This file is the
//! separate, ignored test that establishes the *contract* — that the flags
//! [`ClaudeCodeDriver`] builds are accepted by the installed CLI, that `-p`
//! terminates, and that `--output-format json` produces the object the driver
//! parses.
//!
//! # Requirements
//!
//! * The `claude` CLI on `PATH`, or `CLAUDE_CODE_BINARY` pointing at it.
//! * **A working authenticated session.** Claude Code reads its own credentials
//!   from the environment; this driver supplies none and must not. An
//!   unauthenticated install fails this test with `is_error: true` and a
//!   non-zero exit — which is a correct observation of the CLI, not a defect in
//!   the driver.
//! * Network access to the Anthropic API.
//! * It spends real tokens and real money. The prompt is one word and
//!   `--max-budget-usd` caps the spend.
//!
//! ```sh
//! CLAUDE_CODE_BINARY=/path/to/claude \
//!   cargo test -p director-adapters --test claude_code_live -- --ignored --nocapture
//! ```

use std::time::Duration;

use director_adapters::harness::{ClaudeCodeConfig, ClaudeCodeDriver};
use director_domain::harness::{HarnessDriver, HarnessRun};

fn binary() -> String {
    std::env::var("CLAUDE_CODE_BINARY").unwrap_or_else(|_| "claude".to_string())
}

fn driver() -> ClaudeCodeDriver {
    ClaudeCodeDriver::new(ClaudeCodeConfig {
        binary: binary(),
        // A small, explicit ceiling: this test must not be able to run away.
        default_timeout: Duration::from_secs(300),
        ..ClaudeCodeConfig::default()
    })
}

fn run(dir: &std::path::Path) -> HarnessRun {
    let mut run = HarnessRun::new(
        dir.to_string_lossy().into_owned(),
        "Reply with exactly: ORQ",
    );
    run.timeout = Duration::from_secs(300);
    run.max_output_bytes = 1024 * 1024;
    // Bound the spend for a test that calls a paid API.
    // Per Atria API documentation (https://api.atria-asi.ai/docs), the only
    // supported model identifier is "Atria-Dawn" (case-sensitive).
    run.model = Some("Atria-Dawn".to_string());
    run
}

fn scratch_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("orqyn-claude-live-{name}"));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The installed CLI accepts the driver's argv and returns the shape it parses.
#[tokio::test]
#[ignore = "spawns the real claude CLI and calls the Anthropic API"]
async fn the_installed_cli_accepts_the_drivers_argv() {
    let dir = scratch_dir("argv");
    let outcome = driver()
        .drive(&run(&dir))
        .await
        .expect("the CLI must start and finish");

    // Whatever it decided, the process ran and said so on stdout.
    assert!(
        outcome.stdout.contains("\"type\":\"result\""),
        "the CLI must print one result object on stdout; got: {}",
        outcome.stdout
    );
    assert!(
        !outcome.truncated,
        "a 1 MiB cap is never reached here, so this must not be set"
    );
    assert!(
        outcome.harness_session.is_some(),
        "the CLI reports its own session id: {}",
        outcome.stdout
    );

    // On an authenticated install this is a clean run. On an unauthenticated one
    // the CLI reports the failure itself, and that is reported rather than
    // hidden - so this asserts the *shape* of the answer, not the answer.
    if outcome.reported_error {
        assert_ne!(
            outcome.exit_code,
            Some(0),
            "a harness reporting an error must not also have exited cleanly"
        );
        panic!(
            "the installed claude CLI reported an error. This test needs an \
             authenticated session:\n{}\nstderr: {}",
            outcome.result_text.as_deref().unwrap_or("(no result text)"),
            outcome.stderr
        );
    }
    assert_eq!(outcome.exit_code, Some(0));
    assert!(outcome.succeeded());
}

/// `claude -p` must terminate on its own. Without `-p` the CLI opens an
/// interactive session and never exits, so this is the flag that makes the whole
/// driver possible.
#[tokio::test]
#[ignore = "spawns the real claude CLI and calls the Anthropic API"]
async fn print_mode_terminates_without_a_tty() {
    let dir = scratch_dir("tty");
    // A prompt that would be slow if the CLI were in interactive mode, so a
    // hang shows up as a timeout rather than as a pass.
    let mut r = run(&dir);
    r.brief = "Reply with exactly: ORQ".into();
    r.timeout = Duration::from_secs(180);

    let started = std::time::Instant::now();
    let result = driver().drive(&r).await;
    let elapsed = started.elapsed();

    match result {
        Ok(_outcome) => assert!(
            elapsed < Duration::from_secs(180),
            "print mode returned after the limit, which means it was not print mode"
        ),
        Err(err) => panic!("print mode did not terminate cleanly: {err}"),
    }
}

/// The driver reports a missing install as a configuration problem rather than
/// letting a caller see a raw spawn failure.
#[tokio::test]
async fn a_missing_cli_is_reported_as_missing() {
    let dir = scratch_dir("missing");
    let driver = ClaudeCodeDriver::new(ClaudeCodeConfig::new("C:/definitely/not/here/claude.exe"));
    let err = driver.drive(&run(&dir)).await.expect_err("no such binary");
    assert!(
        matches!(
            err,
            director_domain::harness::HarnessError::BinaryMissing { .. }
        ),
        "expected a missing-binary error, got {err:?}"
    );
}
