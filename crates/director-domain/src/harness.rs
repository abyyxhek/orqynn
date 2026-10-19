//! Driving a real agent harness, and the evidence that comes back.
//!
//! Verification and agent execution are deliberately different things, and this
//! module is only about the second one. [`crate::providers::ExecutionProvider`]
//! runs a *check* and reports an exit code; a [`HarnessDriver`] runs an *agent*
//! and reports what that agent did. Keeping them apart is what lets the
//! verifier's verdict stay independent of whatever the agent claims.
//!
//! # What an outcome is, and is not
//!
//! [`HarnessOutcome`] is an *observation*: a process ran, produced streams, and
//! exited. It is deliberately incapable of concluding that a task succeeded. A
//! harness that exits zero having changed nothing produces an outcome that looks
//! exactly like one that exits zero having done the work, because from out here
//! the two are indistinguishable. The only thing that can tell them apart is
//! verification, run separately against the repository.
//!
//! So [`HarnessOutcome::succeeded`] answers "did the harness process run to
//! completion without reporting an error", and that is the strongest claim in
//! this module. It is not "the task is done".
//!
//! # Why the trait is here and the process is not
//!
//! The trait carries no process, no binary path, and no argv. Those belong to
//! the adapter, because only the adapter knows how one particular harness is
//! invoked. This keeps the domain free of the things that change when a vendor
//! ships a new flag.

use std::time::Duration;

use crate::agent::Harness;

/// One agent invocation: what to run, where, and for how long.
///
/// Deliberately free of credentials and environment overrides. A caller that
/// needs to pass a key to a harness sets it on the driver's configuration, and
/// the driver applies it to the child process without ever copying it into a
/// [`HarnessRun`], a [`HarnessOutcome`], or a log line. That is a structural
/// guarantee rather than a discipline: there is no field here for a secret to
/// land in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessRun {
    /// Directory the harness runs in. Must exist; the driver refuses otherwise.
    pub working_dir: String,
    /// The briefing, rendered from persisted recovery context.
    pub brief: String,
    /// Hard limit on wall-clock time. The child is killed when it elapses.
    pub timeout: Duration,
    /// Bytes of each stream to retain before truncating. Output is evidence, but
    /// an unbounded capture is a memory leak with a process attached.
    pub max_output_bytes: usize,
    /// Additional directories the harness is permitted to reach.
    pub extra_dirs: Vec<String>,
    /// Permission mode, passed through to harnesses that accept one.
    pub permission_mode: Option<String>,
    /// Model override, passed through to harnesses that accept one.
    pub model: Option<String>,
}

impl HarnessRun {
    /// A run with no permission or model override, carrying an explicit
    /// timeout and output bound.
    pub fn new(working_dir: impl Into<String>, brief: impl Into<String>) -> Self {
        HarnessRun {
            working_dir: working_dir.into(),
            brief: brief.into(),
            timeout: Duration::from_secs(900),
            max_output_bytes: 256 * 1024,
            extra_dirs: Vec::new(),
            permission_mode: None,
            model: None,
        }
    }

    /// True when the run is worth launching at all.
    ///
    /// An empty briefing is refused rather than handed to a harness that would
    /// treat silence as "carry on regardless": the whole point of a driver is
    /// that the agent is briefed from persisted records, and launching one with
    /// nothing to read is how an agent starts inventing context.
    pub fn is_runnable(&self) -> Result<(), HarnessError> {
        if self.working_dir.trim().is_empty() {
            return Err(HarnessError::InvalidConfig(
                "no working directory was given".into(),
            ));
        }
        if self.brief.trim().is_empty() {
            return Err(HarnessError::InvalidConfig(
                "the briefing is empty; an agent must not be launched without one".into(),
            ));
        }
        if self.timeout.is_zero() {
            return Err(HarnessError::InvalidConfig(
                "the execution timeout is zero, so the harness could never finish".into(),
            ));
        }
        if self.max_output_bytes == 0 {
            return Err(HarnessError::InvalidConfig(
                "the output limit is zero, so no execution evidence could be captured".into(),
            ));
        }
        Ok(())
    }
}

/// What running a harness produced. Observed, never inferred.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HarnessOutcome {
    /// The process's exit code, or `None` if it was killed before exiting.
    pub exit_code: Option<i32>,
    /// Captured stdout, possibly truncated. See [`Self::truncated`].
    pub stdout: String,
    /// Captured stderr, possibly truncated.
    pub stderr: String,
    /// True when either stream hit the configured byte limit.
    pub truncated: bool,
    /// True when the harness itself reported an error in its own output, even
    /// if the process exited zero.
    pub reported_error: bool,
    /// Wall-clock milliseconds the process ran for.
    pub duration_ms: u64,
    /// The harness's own summary of what it did, when it reports one.
    pub result_text: Option<String>,
    /// The harness's own session identifier, when it reports one.
    ///
    /// This is the harness's identifier, not Orqyn's. Orqyn derives its own
    /// session id from the assignment tenure, and the two are deliberately not
    /// forced to match.
    pub harness_session: Option<String>,
    /// Cost in USD, when the harness reports it.
    pub cost_usd: Option<f64>,
}

impl HarnessOutcome {
    /// True when the process ran to completion and reported no error.
    ///
    /// The strongest claim this type makes, and it is only about the *process*.
    /// A harness can succeed here having achieved nothing; see the module docs.
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && !self.truncated && !self.reported_error
    }
}

/// Every way driving a harness can fail.
///
/// Split by cause rather than collapsed, because the caller's response differs:
/// a missing binary is a configuration problem that recurs on every tick, while
/// a timeout is a one-off that a retry may well survive.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    /// The driver was never given a binary to run.
    #[error("no harness binary is configured: {0}")]
    NotConfigured(String),
    /// The configured binary does not exist.
    #[error("the harness binary {path} does not exist")]
    BinaryMissing {
        /// The configured path that does not exist.
        path: String,
    },
    /// The process could not be started.
    #[error("the harness could not be started: {message}")]
    Spawn {
        /// The operating system's own description of the failure.
        message: String,
    },
    /// The harness outstayed its limit and was killed.
    #[error("the harness exceeded its {limit:?} limit and was killed")]
    Timeout {
        /// The limit that elapsed.
        limit: Duration,
    },
    /// The caller cancelled the drive.
    #[error("the agent drive was cancelled")]
    Cancelled,
    /// The request was not runnable.
    #[error("invalid harness configuration: {0}")]
    InvalidConfig(String),
    /// The working directory does not exist or is not a directory.
    #[error("the working directory {path} is not a usable directory: {problem}")]
    UnusableWorkingDir {
        /// The directory that was asked for.
        path: String,
        /// Why it cannot be used.
        problem: String,
    },
}

/// Launches a real agent harness and reports what it actually did.
///
/// One implementation exists per harness that has been implemented and tested.
/// A [`Harness`] variant with no driver behind it is a domain fact with no way
/// to act on it, and [`Self::harness`] returning `None` says so rather than
/// letting a caller assume any variant is drivable.
#[async_trait::async_trait]
pub trait HarnessDriver: Send + Sync {
    /// Run the harness and observe the result.
    async fn drive(&self, run: &HarnessRun) -> Result<HarnessOutcome, HarnessError>;

    /// A stable name for this driver, recorded alongside its evidence.
    fn harness_name(&self) -> &'static str;

    /// The domain harness this driver speaks to.
    fn harness(&self) -> Option<Harness>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> HarnessRun {
        HarnessRun::new("C:/work", "do the thing")
    }

    #[test]
    fn a_normal_run_is_runnable() {
        assert!(run().is_runnable().is_ok(), "a normal run is runnable");
    }

    #[test]
    fn an_empty_working_directory_is_refused() {
        let mut r = run();
        r.working_dir = "   ".into();
        assert!(matches!(
            r.is_runnable(),
            Err(HarnessError::InvalidConfig(_))
        ));
    }

    #[test]
    fn an_empty_briefing_is_refused_rather_than_silently_sent() {
        let mut r = run();
        r.brief = "\n\t ".into();
        assert!(matches!(
            r.is_runnable(),
            Err(HarnessError::InvalidConfig(m)) if m.contains("briefing is empty")
        ));
    }

    #[test]
    fn a_zero_timeout_is_refused() {
        let mut r = run();
        r.timeout = Duration::ZERO;
        assert!(matches!(
            r.is_runnable(),
            Err(HarnessError::InvalidConfig(_))
        ));
    }

    #[test]
    fn a_zero_output_limit_is_refused() {
        let mut r = run();
        r.max_output_bytes = 0;
        assert!(matches!(
            r.is_runnable(),
            Err(HarnessError::InvalidConfig(_))
        ));
    }

    #[test]
    fn succeeded_means_the_process_ran_cleanly_and_nothing_more() {
        let clean = HarnessOutcome {
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            reported_error: false,
            duration_ms: 5,
            result_text: None,
            harness_session: None,
            cost_usd: None,
        };
        assert!(clean.succeeded());

        // Every one of these is a process that ran and did not report an error.
        for mutate in [
            (|o: &mut HarnessOutcome| o.exit_code = Some(1)) as fn(&mut HarnessOutcome),
            |o: &mut HarnessOutcome| o.exit_code = None,
            |o: &mut HarnessOutcome| o.truncated = true,
            |o: &mut HarnessOutcome| o.reported_error = true,
        ] {
            let mut o = clean.clone();
            mutate(&mut o);
            assert!(!o.succeeded(), "this outcome must not count as a clean run");
        }
    }
}
