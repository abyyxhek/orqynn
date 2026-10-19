//! The Claude Code harness driver.
//!
//! # The invocation contract, as verified against the installed CLI
//!
//! These flags were read from `claude --help` on Claude Code 2.1.250, not
//! assumed. The driver builds:
//!
//! ```text
//! claude -p --output-format json [--model M] [--permission-mode P] [--add-dir D]...
//! ```
//!
//! with the briefing on **stdin**. Three details of that contract are load
//! bearing and are the reason this file exists:
//!
//! * **`-p` is required.** Without it Claude starts an interactive session and
//!   never exits. Every driver invocation is `-p` or it hangs until the timeout.
//! * **The prompt goes on stdin, not argv.** `-p` reads stdin. A briefing
//!   arrives as a single argument instead — works for short strings, and breaks
//!   on Windows for any briefing with a newline in it, which every real
//!   briefing has.
//! * **`--session-id` is not usable.** It is documented as "must be a valid
//!   UUID". Orqyn's session ids are `SESS-{assignment_id}`, so passing one
//!   would be rejected. The driver's own session is therefore recorded by
//!   Orqyn's `monitor::acknowledge` and never pushed into Claude.
//!
//! # Where the evidence comes from
//!
//! `--output-format json` prints **one** JSON object on **stdout**. Diagnostics
//! such as `[claude-code:unrecognized_model]` arrive on **stderr**. The driver
//! keeps the streams separate for exactly that reason: merging them, as a naive
//! implementation does, makes the result unparseable whenever the CLI has
//! anything to say.
//!
//! # Auth
//!
//! The driver never handles credentials. Claude Code reads its own auth from the
//! environment it inherits. The only environment this driver *sets* is what the
//! caller explicitly configured, and none of it is ever copied into an outcome.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use director_domain::agent::Harness;
use director_domain::harness::{HarnessDriver, HarnessError, HarnessOutcome, HarnessRun};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// How to launch Claude Code.
///
/// `env` is applied to the child and nowhere else. It is not part of any
/// serialized type and not reachable from a [`HarnessOutcome`], so a token put
/// here cannot end up in a record or a log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeConfig {
    /// Path to the `claude` executable.
    pub binary: String,
    /// Passed as `--permission-mode`. `None` leaves the CLI's default alone.
    pub permission_mode: Option<String>,
    /// Passed as `--model`.
    pub model: Option<String>,
    /// Applied when a [`HarnessRun`] does not set its own limit.
    pub default_timeout: Duration,
    /// Applied when a [`HarnessRun`] does not set its own limit.
    pub max_output_bytes: usize,
    /// Extra arguments appended verbatim, for harnesses that grow flags.
    pub extra_args: Vec<String>,
    /// Environment applied to the child. Never logged.
    pub env: Vec<(String, String)>,
}

impl Default for ClaudeCodeConfig {
    fn default() -> Self {
        ClaudeCodeConfig {
            binary: "claude".into(),
            permission_mode: None,
            model: None,
            // Generous by default and deliberately so: a measured 401 from this
            // CLI took 177s to surface, because it retries before giving up. A
            // default shorter than that would report a timeout for a request
            // that was merely slow, and the caller would learn nothing.
            default_timeout: Duration::from_secs(900),
            max_output_bytes: 256 * 1024,
            extra_args: Vec::new(),
            env: Vec::new(),
        }
    }
}

impl ClaudeCodeConfig {
    /// A configuration pointing at an explicit binary path.
    pub fn new(binary: impl Into<String>) -> Self {
        ClaudeCodeConfig {
            binary: binary.into(),
            ..ClaudeCodeConfig::default()
        }
    }

    /// Refuse a configuration that could only fail at spawn time.
    pub fn validate(&self) -> Result<(), HarnessError> {
        if self.binary.trim().is_empty() {
            return Err(HarnessError::NotConfigured(
                "the binary path is empty".into(),
            ));
        }
        if self.default_timeout.is_zero() {
            return Err(HarnessError::InvalidConfig(
                "the default execution timeout is zero".into(),
            ));
        }
        if self.max_output_bytes == 0 {
            return Err(HarnessError::InvalidConfig(
                "the default output limit is zero".into(),
            ));
        }
        Ok(())
    }
}

/// The one real harness driver: Claude Code in non-interactive print mode.
#[derive(Debug, Clone)]
pub struct ClaudeCodeDriver {
    config: ClaudeCodeConfig,
}

impl ClaudeCodeDriver {
    /// Build a driver from a validated-when-used configuration.
    pub fn new(config: ClaudeCodeConfig) -> Self {
        ClaudeCodeDriver { config }
    }

    /// The configuration in force.
    pub fn config(&self) -> &ClaudeCodeConfig {
        &self.config
    }

    /// The exact argv this driver would pass, as `OsString`s.
    ///
    /// Arguments are separate argv entries and the briefing is **not** among
    /// them — it goes to stdin. There is no shell anywhere in this path, so a
    /// briefing containing quotes, semicolons, or `$` cannot become syntax.
    pub fn argv(&self, run: &HarnessRun) -> Vec<std::ffi::OsString> {
        let mut argv: Vec<std::ffi::OsString> = Vec::new();
        argv.push(OsStr::new("-p").to_os_string());
        argv.push(OsStr::new("--output-format").to_os_string());
        argv.push(OsStr::new("json").to_os_string());

        let model = run.model.as_ref().or(self.config.model.as_ref());
        if let Some(model) = model {
            argv.push(OsStr::new("--model").to_os_string());
            argv.push(OsStr::new(model).to_os_string());
        }

        let mode = run
            .permission_mode
            .as_ref()
            .or(self.config.permission_mode.as_ref());
        if let Some(mode) = mode {
            argv.push(OsStr::new("--permission-mode").to_os_string());
            argv.push(OsStr::new(mode).to_os_string());
        }

        for dir in &run.extra_dirs {
            argv.push(OsStr::new("--add-dir").to_os_string());
            argv.push(OsStr::new(dir.as_str()).to_os_string());
        }

        argv.extend(
            self.config
                .extra_args
                .iter()
                .map(OsStr::new)
                .map(|s| s.to_os_string()),
        );
        argv
    }
}

/// Whether `name` names a program this machine can run.
///
/// A name carrying a path separator is used as given. A bare name is looked up
/// on `PATH`, with `PATHEXT` honoured on Windows so `claude` finds `claude.exe`
/// — which is what makes the default configuration usable at all.
///
/// Returns whether it resolves. The resolved path is deliberately not returned:
/// the child is still launched by name, and letting the OS resolve it keeps
/// spawn behaviour identical to invoking the command directly.
fn resolve_binary(name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let candidate = Path::new(name);
    if name.contains('/') || name.contains('\\') {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }

    let path_var = std::env::var_os("PATH")?;
    // Windows resolves a bare name through PATHEXT; elsewhere the name is
    // already complete.
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.to_ascii_lowercase())
            .collect()
    } else {
        Vec::new()
    };

    for dir in std::env::split_paths(&path_var) {
        let direct = dir.join(name);
        if direct.is_file() {
            return Some(direct);
        }
        for ext in &extensions {
            let with_ext = dir.join(format!("{name}{ext}"));
            if with_ext.is_file() {
                return Some(with_ext);
            }
        }
    }
    None
}

/// The `--output-format json` result object.
///
/// Every field is optional. The CLI has added fields across versions and this
/// driver is expected to outlive the one it was written against, so a missing
/// key means "not reported" and never "absent".
#[derive(Debug, Default, serde::Deserialize)]
struct ClaudeResult {
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default)]
    duration_ms: Option<u64>,
    /// Present on failures; the machine-readable reason the run stopped.
    #[serde(default)]
    terminal_reason: Option<String>,
}

/// Pull the result object out of stdout.
///
/// Tries the whole stream first, then each line, because a future CLI build may
/// prefix a banner. Returns `None` rather than a default when nothing parses,
/// so an unreadable result is visible rather than silently empty.
fn parse_result(stdout: &str) -> Option<ClaudeResult> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(parsed) = serde_json::from_str::<ClaudeResult>(trimmed) {
        return Some(parsed);
    }
    trimmed
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<ClaudeResult>(line.trim()).ok())
}

#[async_trait::async_trait]
impl HarnessDriver for ClaudeCodeDriver {
    fn harness_name(&self) -> &'static str {
        "claude-code"
    }

    fn harness(&self) -> Option<Harness> {
        Some(Harness::ClaudeCode)
    }

    async fn drive(&self, run: &HarnessRun) -> Result<HarnessOutcome, HarnessError> {
        self.config.validate()?;
        run.is_runnable()?;

        // Checked before spawning so a typo is reported as configuration rather
        // than as an opaque spawn failure. A bare name has to be looked up on
        // PATH: `Path::new("claude").exists()` is false even when the CLI is
        // installed and on PATH, and the default configuration is exactly a
        // bare name.
        if resolve_binary(&self.config.binary).is_none() {
            return Err(HarnessError::BinaryMissing {
                path: self.config.binary.clone(),
            });
        }
        let working_dir = Path::new(&run.working_dir);
        if !working_dir.is_dir() {
            return Err(HarnessError::UnusableWorkingDir {
                path: run.working_dir.clone(),
                problem: "it is not an existing directory".into(),
            });
        }

        let timeout = if run.timeout.is_zero() {
            self.config.default_timeout
        } else {
            run.timeout
        };
        let max_bytes = if run.max_output_bytes == 0 {
            self.config.max_output_bytes
        } else {
            run.max_output_bytes
        };

        let mut child = Command::new(&self.config.binary);
        child
            .args(self.argv(run))
            .current_dir(working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Without this a cancelled drive leaves the harness running.
            .kill_on_drop(true);
        for (key, value) in &self.config.env {
            child.env(key, value);
        }

        let mut child = child.spawn().map_err(|err| HarnessError::Spawn {
            message: err.to_string(),
        })?;

        // The briefing goes in on stdin, and stdin is closed immediately after.
        // Leaving it open would let the CLI wait for more input.
        {
            let mut stdin = child.stdin.take().ok_or_else(|| HarnessError::Spawn {
                message: "the child had no stdin to write the briefing to".into(),
            })?;
            stdin
                .write_all(run.brief.as_bytes())
                .await
                .map_err(|err| HarnessError::Spawn {
                    message: format!("could not write the briefing: {err}"),
                })?;
            stdin.shutdown().await.map_err(|err| HarnessError::Spawn {
                message: format!("could not close the briefing stream: {err}"),
            })?;
        }

        let started = Instant::now();
        let mut stdout = child.stdout.take().expect("stdout was piped");
        let mut stderr = child.stderr.take().expect("stderr was piped");

        let drained = tokio::time::timeout(timeout, async {
            let out = read_capped(&mut stdout, max_bytes).await;
            let err = read_capped(&mut stderr, max_bytes).await;
            let status = child.wait().await;
            (out, err, status)
        })
        .await;

        let (stdout_text, stdout_clipped, stderr_text, stderr_clipped, status) = match drained {
            Ok((o, e, s)) => {
                let s = s.map_err(|err| HarnessError::Spawn {
                    message: format!("the harness could not be waited on: {err}"),
                })?;
                (o.0, o.1, e.0, e.1, s)
            }
            Err(_) => {
                // Out of time: kill and reap so no orphan outlives the drive.
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(HarnessError::Timeout { limit: timeout });
            }
        };

        let exit_code = status.code();
        let parsed = parse_result(&stdout_text);

        // A harness that says `is_error` is believed even on exit 0: the exit
        // code and the harness's own account are separate facts, and reporting
        // only the tidier one would be the optimistic read.
        let reported_error = parsed.as_ref().map(|p| p.is_error).unwrap_or(false);

        let result_text = parsed.as_ref().and_then(|p| p.result.clone());
        let harness_session = parsed.as_ref().and_then(|p| p.session_id.clone());
        let cost_usd = parsed.as_ref().and_then(|p| p.total_cost_usd);
        let terminal_reason = parsed.as_ref().and_then(|p| p.terminal_reason.clone());
        // The CLI times its own turn. Prefer it over the driver's wall clock when
        // both exist: it excludes process startup, and it is the number the
        // harness's own account agrees with.
        let reported_duration = parsed.as_ref().and_then(|p| p.duration_ms);
        let duration_ms = reported_duration.unwrap_or_else(|| started.elapsed().as_millis() as u64);

        let mut stderr_text = stderr_text;
        // A failure's machine-readable stop reason is the single most useful
        // thing the CLI emits, and on some failures stderr stays empty. Fold it
        // into stderr so it survives as evidence rather than being parsed and
        // discarded.
        if reported_error {
            if let Some(reason) = terminal_reason {
                if !stderr_text.contains(&reason) {
                    if !stderr_text.is_empty() {
                        stderr_text.push('\n');
                    }
                    stderr_text.push_str(&format!("harness reported: {reason}"));
                }
            }
        }

        Ok(HarnessOutcome {
            exit_code,
            stdout: stdout_text,
            stderr: stderr_text,
            truncated: stdout_clipped || stderr_clipped,
            reported_error,
            duration_ms,
            result_text,
            harness_session,
            cost_usd,
        })
    }
}

/// Read a stream to completion, keeping at most `cap` bytes.
///
/// Reports whether the stream was cut short, so a caller can tell a complete
/// capture from a sample of one.
async fn read_capped<R>(reader: &mut R, cap: usize) -> (String, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::with_capacity(cap.min(64 * 1024));
    let mut chunk = [0u8; 16 * 1024];
    let mut clipped = false;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let room = cap - buf.len();
                    let take = room.min(n);
                    buf.extend_from_slice(&chunk[..take]);
                    if take < n {
                        clipped = true;
                    }
                } else {
                    // Keep draining even though the bytes are past the cap.
                    // Stopping here would leave the child writing into a pipe
                    // nobody reads, and it would block forever — which surfaces
                    // as a timeout rather than as the output limit it is.
                    clipped = true;
                }
            }
            Err(_) => break,
        }
    }
    (String::from_utf8_lossy(&buf).into_owned(), clipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> HarnessRun {
        HarnessRun::new("C:/work", "do the thing")
    }

    #[test]
    fn argv_carries_the_flags_the_verified_contract_requires() {
        let driver = ClaudeCodeDriver::new(ClaudeCodeConfig::new("claude"));
        let argv: Vec<String> = driver
            .argv(&run())
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv, vec!["-p", "--output-format", "json"]);
        assert!(
            !argv.iter().any(|a| a.contains("do the thing")),
            "the briefing must never appear in argv"
        );
        assert!(
            !argv.iter().any(|a| a == "--session-id"),
            "--session-id requires a UUID and Orqyn's ids are not UUIDs"
        );
    }

    #[test]
    fn optional_flags_appear_only_when_configured() {
        let driver = ClaudeCodeDriver::new(ClaudeCodeConfig {
            permission_mode: Some("acceptEdits".into()),
            model: Some("sonnet".into()),
            ..ClaudeCodeConfig::new("claude")
        });
        let mut r = run();
        r.extra_dirs = vec!["C:/work/vendor".into()];
        let argv: Vec<String> = driver
            .argv(&r)
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv,
            vec![
                "-p",
                "--output-format",
                "json",
                "--model",
                "sonnet",
                "--permission-mode",
                "acceptEdits",
                "--add-dir",
                "C:/work/vendor",
            ]
        );
    }

    #[test]
    fn a_run_level_override_beats_the_configuration() {
        let driver = ClaudeCodeDriver::new(ClaudeCodeConfig {
            model: Some("from-config".into()),
            ..ClaudeCodeConfig::new("claude")
        });
        let mut r = run();
        r.model = Some("from-run".into());
        let argv: Vec<String> = driver
            .argv(&r)
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(argv.contains(&"from-run".to_string()));
        assert!(!argv.contains(&"from-config".to_string()));
    }

    #[test]
    fn a_briefing_full_of_shell_metacharacters_stays_out_of_argv() {
        let driver = ClaudeCodeDriver::new(ClaudeCodeConfig::new("claude"));
        let mut r = run();
        r.brief = "rm -rf / && echo \"$HOME\"; whoami".into();
        let argv: Vec<String> = driver
            .argv(&r)
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv, vec!["-p", "--output-format", "json"]);
    }

    #[test]
    fn configuration_is_refused_before_it_can_fail_at_spawn() {
        let empty = ClaudeCodeConfig {
            binary: "  ".into(),
            ..ClaudeCodeConfig::default()
        };
        assert!(matches!(
            empty.validate(),
            Err(HarnessError::NotConfigured(_))
        ));

        let no_timeout = ClaudeCodeConfig {
            default_timeout: Duration::ZERO,
            ..ClaudeCodeConfig::new("claude")
        };
        assert!(matches!(
            no_timeout.validate(),
            Err(HarnessError::InvalidConfig(_))
        ));

        let no_output = ClaudeCodeConfig {
            max_output_bytes: 0,
            ..ClaudeCodeConfig::new("claude")
        };
        assert!(matches!(
            no_output.validate(),
            Err(HarnessError::InvalidConfig(_))
        ));
    }

    #[test]
    fn a_real_result_object_parses() {
        let stdout = r#"{"is_error":false,"result":"done","session_id":"abc-123","total_cost_usd":0.02,"duration_ms":900,"type":"result"}"#;
        let parsed = parse_result(stdout).expect("parses");
        assert!(!parsed.is_error);
        assert_eq!(parsed.result.as_deref(), Some("done"));
        assert_eq!(parsed.session_id.as_deref(), Some("abc-123"));
        assert_eq!(parsed.total_cost_usd, Some(0.02));
    }

    #[test]
    fn a_result_object_after_a_banner_still_parses() {
        let stdout = "[claude-code:some-warning] noisy\n{\"is_error\":false,\"result\":\"ok\"}";
        let parsed = parse_result(stdout).expect("the JSON line is still found");
        assert_eq!(parsed.result.as_deref(), Some("ok"));
    }

    #[test]
    fn unparseable_output_is_none_rather_than_a_default() {
        assert!(parse_result("").is_none());
        assert!(parse_result("   ").is_none());
        assert!(parse_result("not json at all").is_none());
    }

    #[tokio::test]
    async fn a_capped_read_stops_at_the_limit_and_says_so() {
        let data = vec![b'x'; 5000];
        let mut cursor = std::io::Cursor::new(data);
        let (text, clipped) = read_capped(&mut cursor, 100).await;
        assert_eq!(text.len(), 100);
        assert!(clipped, "a truncated capture must announce itself");
    }

    #[tokio::test]
    async fn a_read_under_the_limit_is_not_reported_as_clipped() {
        let data = b"short".to_vec();
        let mut cursor = std::io::Cursor::new(data);
        let (text, clipped) = read_capped(&mut cursor, 100).await;
        assert_eq!(text, "short");
        assert!(!clipped);
    }

    #[tokio::test]
    async fn a_missing_binary_is_reported_as_missing_not_as_a_spawn_failure() {
        let driver = ClaudeCodeDriver::new(ClaudeCodeConfig::new(
            "C:/definitely/not/here/claude-xyz.exe",
        ));
        let err = driver
            .drive(&run())
            .await
            .expect_err("the binary does not exist");
        assert!(
            matches!(err, HarnessError::BinaryMissing { .. }),
            "expected a missing-binary error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn an_unusable_working_directory_is_refused() {
        let driver = ClaudeCodeDriver::new(ClaudeCodeConfig::new("claude"));
        let mut r = run();
        r.working_dir = "C:/definitely/not/here".into();
        let err = driver.drive(&r).await.expect_err("no such directory");
        assert!(
            matches!(err, HarnessError::UnusableWorkingDir { .. }),
            "expected an unusable-directory error, got {err:?}"
        );
    }

    #[test]
    fn a_bare_name_is_resolved_on_path_rather_than_treated_as_a_missing_file() {
        // The default configuration is a bare name, and
        // `Path::new("claude").exists()` is false even when the CLI is
        // installed. Resolving on PATH is what makes the default usable.
        let default = ClaudeCodeConfig::default();
        assert_eq!(default.binary, "claude");
        // Whether it resolves depends on the machine, but a name that IS on PATH
        // must resolve, and one that cannot exist must not.
        assert_eq!(
            resolve_binary("definitely-not-a-real-binary-xyz"),
            None,
            "a name no machine has must not resolve"
        );
        assert_eq!(resolve_binary(""), None, "an empty name must not resolve");
        assert_eq!(
            resolve_binary("C:/definitely/not/here/claude-xyz.exe"),
            None,
            "an explicit path is taken as given and must not resolve"
        );
    }

    #[test]
    fn an_explicit_relative_path_with_a_separator_is_not_searched_for_on_path() {
        assert_eq!(
            resolve_binary("./not-a-real-binary"),
            None,
            "a path with a separator is used verbatim, so PATH must not rescue it"
        );
    }

    #[tokio::test]
    async fn an_empty_briefing_never_reaches_the_process() {
        let driver = ClaudeCodeDriver::new(ClaudeCodeConfig::new("claude"));
        let mut r = run();
        r.brief = "  ".into();
        let err = driver.drive(&r).await.expect_err("refused");
        assert!(matches!(err, HarnessError::InvalidConfig(_)));
    }
}
