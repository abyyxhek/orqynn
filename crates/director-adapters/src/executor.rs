//! [`LocalExecutor`] — run real commands on this machine.
//!
//! This is the fallback [`ExecutionProvider`](director_domain::providers::ExecutionProvider)
//! and the reference implementation. It exists so that Director's verification
//! engine never has to trust an agent's say-so: when a substrate cannot execute
//! commands, Director runs the test suite itself.
//!
//! ## Honest limitations
//!
//! `run_command` here is **synchronous under the hood**. It blocks the async
//! executor thread for the duration of the command, up to the timeout. That is
//! fine for a bounded test command and wrong for a long-running build server.
//! Phase 10 wraps this in `spawn_blocking`; the trait is already `async`, so
//! that change touches only this file.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use director_domain::providers::{CommandOutcome, CommandSpec, ExecutionProvider, Provider};

/// Default ceiling on how long a single command may run.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Errors a local execution can fail with.
#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    /// The program could not be started (not on `PATH`, not executable).
    #[error("could not spawn {program}: {source}")]
    SpawnFailed {
        /// The program that could not be started, e.g. `cargo`.
        program: String,
        #[source]
        /// The underlying OS error.
        source: std::io::Error,
    },
}

/// Runs commands on the local machine.
#[derive(Debug, Clone)]
pub struct LocalExecutor {
    timeout: Duration,
}

impl Default for LocalExecutor {
    fn default() -> Self {
        LocalExecutor {
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl LocalExecutor {
    /// Build an executor with a custom timeout.
    pub fn with_timeout(timeout: Duration) -> Self {
        LocalExecutor { timeout }
    }
}

impl Provider for LocalExecutor {
    type Error = ExecutorError;
}

#[async_trait]
impl ExecutionProvider for LocalExecutor {
    async fn run_command(&self, spec: &CommandSpec) -> Result<CommandOutcome, Self::Error> {
        let mut command = Command::new(&spec.program);
        command.args(&spec.args);
        if let Some(dir) = &spec.working_dir {
            command.current_dir(dir);
        }
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|source| ExecutorError::SpawnFailed {
                program: spec.program.clone(),
                source,
            })?;

        // Poll for completion so we can enforce the timeout. `try_wait` does
        // not block, and the sleep keeps this from becoming a busy loop.
        let started = Instant::now();
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                let output = child.wait_with_output().unwrap_or_else(|_| {
                    // The child already exited; if output collection somehow
                    // fails, report empty streams rather than lose the exit code.
                    std::process::Output {
                        status,
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    }
                });
                return Ok(CommandOutcome {
                    exit_code: status.code().unwrap_or(-1),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    timed_out: false,
                });
            }
            if started.elapsed() >= self.timeout {
                // A timeout is a *result*, not an infrastructure failure: the
                // verification engine must be able to observe "the suite hung"
                // and act on it. Kill the child, keep whatever it produced.
                let _ = child.kill();
                let output = child
                    .wait_with_output()
                    .unwrap_or_else(|_| std::process::Output {
                        status: std::process::ExitStatus::default(),
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    });
                return Ok(CommandOutcome {
                    exit_code: -1,
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    timed_out: true,
                });
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_program_is_a_spawn_failure() {
        let exec = LocalExecutor::default();
        let spec = CommandSpec {
            program: "definitely-not-a-real-program-9f3a".into(),
            args: vec![],
            working_dir: None,
        };
        // Synchronous wrapper for the test: run the future to completion on the
        // current thread via a minimal runtime shim.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(exec.run_command(&spec)).unwrap_err();
        assert!(matches!(err, ExecutorError::SpawnFailed { .. }));
    }

    #[test]
    fn a_successful_command_reports_exit_zero() {
        let exec = LocalExecutor::default();
        let spec = CommandSpec {
            program: if cfg!(windows) {
                "cmd".into()
            } else {
                "true".into()
            },
            args: if cfg!(windows) {
                vec!["/C".into(), "exit".into(), "0".into()]
            } else {
                vec![]
            },
            working_dir: None,
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = rt.block_on(exec.run_command(&spec)).unwrap();
        assert!(outcome.succeeded());
    }

    #[test]
    fn a_failing_command_reports_its_exit_code() {
        let exec = LocalExecutor::default();
        let spec = CommandSpec {
            program: if cfg!(windows) {
                "cmd".into()
            } else {
                "false".into()
            },
            args: if cfg!(windows) {
                vec!["/C".into(), "exit".into(), "3".into()]
            } else {
                vec![]
            },
            working_dir: None,
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = rt.block_on(exec.run_command(&spec)).unwrap();
        assert!(!outcome.succeeded());
    }

    #[test]
    fn a_hanging_command_times_out_as_an_outcome_not_an_error() {
        // The verification engine needs to observe "the suite hung" as a
        // result it can act on, not as an infrastructure failure.
        let exec = LocalExecutor::with_timeout(Duration::from_millis(150));
        let spec = CommandSpec {
            // Something that blocks for a while, whichever platform we are on.
            program: if cfg!(windows) {
                "ping".into()
            } else {
                "sleep".into()
            },
            args: if cfg!(windows) {
                vec!["-n".into(), "10".into(), "127.0.0.1".into()]
            } else {
                vec!["10".into()]
            },
            working_dir: None,
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = rt.block_on(exec.run_command(&spec)).unwrap();

        assert!(outcome.timed_out, "expected a timeout outcome");
        assert!(!outcome.succeeded());
    }
}
