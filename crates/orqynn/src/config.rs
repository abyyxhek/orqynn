//! Argument parsing and configuration for the `orqynn` binary.
//!
//! Split out of `main.rs` so the parts worth testing — parsing, validation, and
//! the safety defaults — can be tested without a process, and so the executable
//! target stays a thin shell around them.
//!
//! # Why hand-rolled
//!
//! The whole command surface is one subcommand with a handful of flags. A CLI
//! framework would add a dependency, a derive, and a version-negotiation surface
//! to express what [`RunArgs::parse`] does in a page, so the parsing is written
//! out. It is strict: an unknown flag is an error rather than something ignored,
//! because silently ignoring a mistyped `--chek` would drop a verification check
//! and make the run look more trustworthy than it is.
//!
//! # Nothing here reads the environment implicitly
//!
//! Environment variables are read only by [`RunArgs::from_env_and_args`], which
//! the caller invokes explicitly, and the precedence is documented there:
//! arguments win over the environment, and the environment wins over the
//! defaults.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Which of the two commands was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Plan a new task and drive it. What Phase 7 added.
    Run,
    /// Recover an existing task from persisted state.
    Resume,
    /// List the persisted handoff records for one task. Read-only.
    Handoffs,
}

impl Command {
    fn parse(token: &str) -> Option<Command> {
        match token {
            "run" => Some(Command::Run),
            "resume" => Some(Command::Resume),
            "handoffs" => Some(Command::Handoffs),
            _ => None,
        }
    }

    /// The word a usage message uses for this command.
    pub fn as_str(self) -> &'static str {
        match self {
            Command::Run => "run",
            Command::Resume => "resume",
            Command::Handoffs => "handoffs",
        }
    }
}

/// A remediation the caller is explicitly authorising for a resumed task.
///
/// Declared here rather than reusing `director_app::replan::Remediation` so that
/// argument parsing does not need to know what a remediation *means*; the
/// lifecycle maps this onto the real one, where the domain rules live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remediation {
    /// Put the task back to `todo` and hand it out again.
    Retry,
    /// Put it back to `todo` with an amended contract.
    Rework,
    /// Mark it `cancelled`.
    Cancel,
}

impl Remediation {
    fn parse(token: &str) -> Option<Remediation> {
        match token {
            "retry" => Some(Remediation::Retry),
            "rework" => Some(Remediation::Rework),
            "cancel" => Some(Remediation::Cancel),
            _ => None,
        }
    }

    /// The real remediation, for `director_app`.
    pub fn to_app(self) -> director_app::replan::Remediation {
        match self {
            // A resume never invents an amendment; a rework with nothing amended
            // is a retry wearing a different name, and replan refuses it as one.
            Remediation::Retry => director_app::replan::Remediation::Retry,
            Remediation::Rework => director_app::replan::Remediation::Rework {
                objective: None,
                expected_outputs: None,
            },
            Remediation::Cancel => director_app::replan::Remediation::Cancel,
        }
    }

    /// Whether this remediation would start a new agent tenure.
    pub fn re_drives(self) -> bool {
        matches!(self, Remediation::Retry | Remediation::Rework)
    }
}

/// Everything wrong with an invocation, said plainly.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    /// A required flag was not supplied.
    #[error("missing required option {0}")]
    Missing(&'static str),
    /// A flag was supplied with no value.
    #[error("{flag} was supplied with no value")]
    EmptyValue {
        /// The flag at fault.
        flag: &'static str,
    },
    /// A flag Orqyn does not define.
    #[error("unrecognised option {0:?}")]
    Unknown(String),
    /// A positional argument where none is accepted.
    #[error("unexpected argument {0:?}; every input is a named option")]
    Unexpected(String),
    /// A repeated flag, for a flag that may appear once.
    #[error("{flag} was supplied more than once")]
    Repeated {
        /// The flag at fault.
        flag: &'static str,
    },
    /// The repository path is not usable.
    #[error("{path} is not a directory")]
    NotADirectory {
        /// The path that was rejected.
        path: String,
    },
    /// The path exists but holds no git repository.
    #[error("{path} is not inside a git repository; Orqyn observes real git state and will not invent it")]
    NotAGitRepository {
        /// The path that was rejected.
        path: String,
    },
    /// The database location cannot be used.
    #[error("the database path {path} is unusable: {problem}")]
    UnusableDatabase {
        /// The path that was rejected.
        path: String,
        /// Why.
        problem: String,
    },
    /// The agent executable is missing.
    #[error("the agent binary {path} does not exist; set --agent-binary or put it on PATH")]
    AgentBinaryMissing {
        /// The path that was rejected.
        path: String,
    },
    /// The agent binary was given as a bare name that is not on PATH.
    #[error(
        "the agent binary {name:?} was not found on PATH; give a full path with --agent-binary"
    )]
    AgentBinaryNotOnPath {
        /// The name that was not resolved.
        name: String,
    },
    /// Memory was asked for but not configured.
    #[error("--memory was requested but AI_MEMORY_BINARY and AI_MEMORY_DATA_DIR are not both set")]
    MemoryNotConfigured,
    /// A flag whose value makes no sense.
    #[error("{flag} must be {expected}")]
    OutOfRange {
        /// The flag at fault.
        flag: &'static str,
        /// What was required.
        expected: &'static str,
    },
    /// The run was asked to execute but no verification check was given.
    #[error("no verification check was supplied; pass --check at least once, or use --dry-run\n\
             a task with no checks can never be verified, so a run without them cannot report success")]
    NoVerificationChecks,
    /// `resume` was asked for without the id of the task to resume.
    #[error("resume needs --task-id naming the task to recover; it will not create a new one")]
    ResumeNeedsTaskId,
    /// The remediation word is not one Orqyn defines.
    #[error("{0:?} is not a remediation; use retry, rework or cancel")]
    UnknownRemediation(String),
    /// A remediation was asked for on a command that has no use for one.
    #[error("--remediate applies to `resume`, not to `run`")]
    RemediationOnRun,
    /// `--dry-run` on a command that has nothing to rehearse.
    #[error(
        "--dry-run applies to `run`; a resume decides from persisted state and starts nothing"
    )]
    DryRunOnResume,
}

/// One parsed invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunArgs {
    /// Which command was asked for.
    pub command: Command,
    /// Repository to observe and work in.
    pub repo: PathBuf,
    /// The task, in the operator's words.
    pub task: String,
    /// Identifier for the task. Derived from `task` when not given.
    pub task_id: String,
    /// Commands VERIFY will run. At least one, unless `--dry-run`.
    pub checks: Vec<String>,
    /// Where SQLite state lives.
    pub database: PathBuf,
    /// The agent executable, or a bare name resolved on PATH.
    pub agent_binary: String,
    /// Harness permission mode. `None` leaves the harness's own default.
    pub permission_mode: Option<String>,
    /// Wall-clock limit for the agent process.
    pub agent_timeout: Duration,
    /// Agent model override.
    pub model: Option<String>,
    /// Extra directories the agent may reach.
    pub extra_dirs: Vec<String>,
    /// Whether to consult and update external memory.
    pub memory: bool,
    /// Validate and report without launching anything.
    pub dry_run: bool,
    /// Wall-clock limit on external-memory retrieval.
    pub memory_timeout: Duration,
    /// An explicitly authorised remediation, for `resume`.
    pub remediate: Option<Remediation>,
}

impl RunArgs {
    /// Defaults a caller starts from.
    pub fn defaults() -> Self {
        RunArgs {
            command: Command::Run,
            repo: PathBuf::new(),
            task: String::new(),
            task_id: String::new(),
            checks: Vec::new(),
            database: PathBuf::new(),
            agent_binary: "claude".into(),
            permission_mode: None,
            agent_timeout: Duration::from_secs(1800),
            model: None,
            extra_dirs: Vec::new(),
            memory: false,
            dry_run: false,
            memory_timeout: Duration::from_secs(5),
            remediate: None,
        }
    }

    /// Parse arguments, then fill anything absent from the environment, then the
    /// defaults.
    ///
    /// Precedence is **arguments, then environment, then default**. The
    /// environment names are read here and nowhere else, so a test can see every
    /// one of them in one place.
    ///
    /// No secret is ever read. `ANTHROPIC_API_KEY` and friends are deliberately
    /// ignored: the agent reads its own credentials, and a CLI that copied them
    /// into its configuration would be one more place they could leak.
    pub fn from_env_and_args<I, S>(args: I, env: &dyn EnvSource) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut parsed = RunArgs::parse(args)?;

        if parsed.repo.as_os_str().is_empty() {
            if let Some(value) = env.get("ORQYNN_REPO") {
                parsed.repo = PathBuf::from(value);
            }
        }
        if parsed.database.as_os_str().is_empty() {
            if let Some(value) = env.get("ORQYNN_DB") {
                parsed.database = PathBuf::from(value);
            }
        }
        if parsed.task.is_empty() {
            if let Some(value) = env.get("ORQYNN_TASK") {
                parsed.task = value;
            }
        }
        if parsed.agent_binary == "claude" {
            if let Some(value) = env.get("ORQYNN_AGENT_BINARY") {
                parsed.agent_binary = value;
            }
        }
        if parsed.permission_mode.is_none() {
            if let Some(value) = env.get("ORQYNN_AGENT_PERMISSION_MODE") {
                parsed.permission_mode = Some(value);
            }
        }
        if parsed.model.is_none() {
            if let Some(value) = env.get("ORQYNN_AGENT_MODEL") {
                parsed.model = Some(value);
            }
        }
        Ok(parsed)
    }

    /// Parse the argument vector.
    ///
    /// A leading token that does not start with `-` is treated as the program
    /// name and skipped, which is how `std::env::args()` presents it. A vector
    /// that begins with a flag is parsed as flags only, so a test does not have
    /// to invent a program name.
    ///
    /// Missing required options are *not* reported here: the environment has not
    /// been consulted yet, and it is a legitimate source for them. They are
    /// reported by [`RunArgs::resolve`], once every source has been merged.
    pub fn parse<I, S>(args: I) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let tokens: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
        // Drop the program name, which is how `std::env::args()` presents it.
        // A vector that begins with a flag is treated as flags only, so a test
        // need not invent a program name.
        let tokens = match tokens.split_first() {
            Some((first, rest)) if !first.starts_with('-') => rest.to_vec(),
            _ => tokens,
        };
        // Then the subcommand. Written out rather than inferred, so the surface does
        // not grow by accident; absent, `run` is assumed for backward
        // compatibility with the Phase 7 invocation.
        let (command, tokens) = match tokens.split_first() {
            Some((first, rest)) => match Command::parse(first) {
                Some(command) => (command, rest.to_vec()),
                None => (Command::Run, tokens),
            },
            None => (Command::Run, Vec::new()),
        };

        let mut args = RunArgs::defaults();
        args.command = command;
        let mut seen: Vec<&'static str> = Vec::new();
        let mut index = 0usize;

        // `--flag=value` is accepted alongside `--flag value`, because both are
        // what people type and rejecting one would be pedantry rather than safety.
        while index < tokens.len() {
            let token = tokens[index].clone();
            index += 1;
            let (name, inline) = match token.split_once('=') {
                Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
                _ => (token.clone(), None),
            };

            let flag: &'static str = match name.as_str() {
                "--repo" => "--repo",
                "--task" => "--task",
                "--task-id" => "--task-id",
                "--check" => "--check",
                "--db" => "--db",
                "--agent-binary" => "--agent-binary",
                "--permission-mode" => "--permission-mode",
                "--model" => "--model",
                "--add-dir" => "--add-dir",
                "--agent-timeout" => "--agent-timeout",
                "--memory-timeout" => "--memory-timeout",
                "--memory" => "--memory",
                "--dry-run" => "--dry-run",
                "--remediate" => "--remediate",
                other => {
                    return Err(if other.starts_with('-') {
                        ConfigError::Unknown(other.to_string())
                    } else {
                        ConfigError::Unexpected(other.to_string())
                    })
                }
            };

            let mut take = |flag: &'static str| -> Result<String, ConfigError> {
                if let Some(value) = inline.clone() {
                    if value.is_empty() {
                        return Err(ConfigError::EmptyValue { flag });
                    }
                    return Ok(value);
                }
                match tokens.get(index) {
                    Some(value) if !value.is_empty() => {
                        index += 1;
                        Ok(value.clone())
                    }
                    _ => Err(ConfigError::EmptyValue { flag }),
                }
            };

            // Flags that may appear once are checked, so a doubled `--repo` is
            // reported rather than silently resolved to the last one.
            let once = flag != "--check" && flag != "--add-dir";
            if once {
                if seen.contains(&flag) {
                    return Err(ConfigError::Repeated { flag });
                }
                seen.push(flag);
            }

            match flag {
                "--repo" => args.repo = PathBuf::from(take(flag)?),
                "--task" => args.task = take(flag)?,
                "--task-id" => args.task_id = take(flag)?,
                "--check" => args.checks.push(take(flag)?),
                "--db" => args.database = PathBuf::from(take(flag)?),
                "--agent-binary" => args.agent_binary = take(flag)?,
                "--permission-mode" => args.permission_mode = Some(take(flag)?),
                "--model" => args.model = Some(take(flag)?),
                "--add-dir" => args.extra_dirs.push(take(flag)?),
                "--memory" => args.memory = true,
                "--dry-run" => args.dry_run = true,
                "--remediate" => {
                    let word = take(flag)?;
                    args.remediate = Some(
                        Remediation::parse(&word)
                            .ok_or_else(|| ConfigError::UnknownRemediation(word.clone()))?,
                    );
                }
                "--agent-timeout" => {
                    let seconds = parse_seconds(take(flag)?, flag)?;
                    args.agent_timeout = Duration::from_secs(seconds);
                }
                "--memory-timeout" => {
                    let seconds = parse_seconds(take(flag)?, flag)?;
                    args.memory_timeout = Duration::from_secs(seconds);
                }
                _ => unreachable!("every flag is matched above"),
            }
        }

        Ok(args)
    }
    /// Fill in what can be derived, and refuse what cannot.
    ///
    /// Called after parsing and after the environment has had its chance. A
    /// `dry_run` is exempt from needing a check, because it will not verify
    /// anything — but it is *not* exempt from needing a repository, a task, or a
    /// database, because it reports on exactly those.
    pub fn resolve(&mut self) -> Result<(), ConfigError> {
        if self.repo.as_os_str().is_empty() {
            return Err(ConfigError::Missing("--repo"));
        }
        if self.command == Command::Resume {
            return self.resolve_resume();
        }
        if self.command == Command::Handoffs {
            return self.resolve_handoffs();
        }
        if self.task.trim().is_empty() {
            return Err(ConfigError::Missing("--task"));
        }
        if self.task_id.trim().is_empty() {
            self.task_id = derive_task_id(&self.task);
        }
        if self.remediate.is_some() {
            return Err(ConfigError::RemediationOnRun);
        }
        if self.database.as_os_str().is_empty() {
            // Next to the repository, and named predictably, rather than in a
            // per-user directory Orqyn would have to be told about.
            self.database = self.repo.join(".orqynn").join("orqynn.db");
        }
        if self.checks.is_empty() && !self.dry_run {
            return Err(ConfigError::NoVerificationChecks);
        }
        // `--memory` itself needs no check here: the substrate's required
        // variables are validated by `AiMemoryConfig`, which names the one that
        // is missing, before anything is launched.
        self.validate_paths()?;
        Ok(())
    }

    /// The rules for `handoffs`, which is a read-only audit listing.
    ///
    /// Nothing here launches anything or requires an agent: this command only reads.
    /// The one field it insists on is `--task-id`, because the question being asked
    /// is always "what happened to *this* task".
    fn resolve_handoffs(&mut self) -> Result<(), ConfigError> {
        if self.task_id.trim().is_empty() {
            return Err(ConfigError::ResumeNeedsTaskId);
        }
        if self.database.as_os_str().is_empty() {
            self.database = self.repo.join(".orqynn").join("orqynn.db");
        }
        // No agent binary is needed to list a record, and no check either.
        self.validate_repository_and_database()?;
        Ok(())
    }

    /// The rules that apply to `resume`, which differ from `run` in three ways:
    /// the task is named rather than described, checks are optional because the
    /// persisted ones are authoritative, and the agent binary is not required
    /// until something is actually launched.
    fn resolve_resume(&mut self) -> Result<(), ConfigError> {
        if self.dry_run {
            return Err(ConfigError::DryRunOnResume);
        }
        if self.task_id.trim().is_empty() {
            return Err(ConfigError::ResumeNeedsTaskId);
        }
        if self.database.as_os_str().is_empty() {
            self.database = self.repo.join(".orqynn").join("orqynn.db");
        }
        // Only the repository and the database are checked here. The agent binary
        // is checked immediately before it would be launched, because a resume
        // that only re-verifies needs no agent and must not be refused for want of
        // one.
        self.validate_repository_and_database()?;
        Ok(())
    }

    /// Check the repository, database and agent binary.
    pub fn validate_paths(&self) -> Result<(), ConfigError> {
        self.validate_repository_and_database()?;
        self.validate_agent_binary()
    }

    /// Check that the repository exists and is a git repository, and that the
    /// database could be created where it was asked for.
    pub fn validate_repository_and_database(&self) -> Result<(), ConfigError> {
        if !self.repo.is_dir() {
            return Err(ConfigError::NotADirectory {
                path: self.repo.display().to_string(),
            });
        }
        if !self.repo.join(".git").exists() {
            return Err(ConfigError::NotAGitRepository {
                path: self.repo.display().to_string(),
            });
        }

        // The database's own directory need not exist — Orqyn creates its own
        // state directory at startup. What must be true is that the *nearest
        // existing ancestor* is a writable directory, because that is where the
        // directory will have to be created. Checking only the immediate parent
        // would reject a perfectly good first run on a fresh clone.
        //
        // An ancestor that exists but is not a directory is a hard stop, not
        // something to walk past: `notes/readme.md/orqynn.db` cannot be created
        // however writable its grandparent is, and walking past it would defer
        // the failure to store-open time.
        let mut ancestor: Option<&Path> = self.database.parent();
        let mut existing: Option<&Path> = None;
        while let Some(candidate) = ancestor {
            if candidate.as_os_str().is_empty() {
                break;
            }
            if candidate.is_dir() {
                existing = Some(candidate);
                break;
            }
            if candidate.exists() {
                return Err(ConfigError::UnusableDatabase {
                    path: self.database.display().to_string(),
                    problem: format!(
                        "{} is a file, and nothing can be created inside it",
                        candidate.display()
                    ),
                });
            }
            ancestor = candidate.parent();
        }
        match existing {
            Some(dir) => {
                let probe = dir.join(".orqynn-write-probe");
                match std::fs::write(&probe, b"") {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&probe);
                    }
                    Err(err) => {
                        return Err(ConfigError::UnusableDatabase {
                            path: self.database.display().to_string(),
                            problem: format!("{} could not be created there: {err}", dir.display()),
                        })
                    }
                }
            }
            None => {
                return Err(ConfigError::UnusableDatabase {
                    path: self.database.display().to_string(),
                    problem: "no existing parent directory could hold it".into(),
                })
            }
        }
        Ok(())
    }

    /// Check the agent binary: a path must exist, a bare name must be on `PATH`.
    ///
    /// Checked separately from the repository and database because a `resume` that
    /// only re-verifies never launches anything, and must not be refused for want
    /// of an executable it was never going to run.
    pub fn validate_agent_binary(&self) -> Result<(), ConfigError> {
        // An agent binary given as a path must exist; one given as a bare name is
        // resolved on PATH, which is how `claude` is normally invoked.
        let candidate = Path::new(&self.agent_binary);
        if candidate.components().count() > 1 || candidate.is_absolute() {
            if !candidate.is_file() {
                return Err(ConfigError::AgentBinaryMissing {
                    path: self.agent_binary.clone(),
                });
            }
        } else if !on_path(&self.agent_binary) {
            return Err(ConfigError::AgentBinaryNotOnPath {
                name: self.agent_binary.clone(),
            });
        }
        Ok(())
    }

    /// The resolved task id.
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    /// A one-line-per-item description of the resolved invocation.
    ///
    /// Reports the resolved values and, for each, whether it was set or left to a
    /// default. It never reads a value from the environment to display it, so there
    /// is no path by which a secret in the environment could reach this output.
    pub fn provenance(&self) -> Vec<String> {
        let mut lines = vec![
            format!("command: {}", self.command.as_str()),
            format!("repository: {}", self.repo.display()),
            format!("database: {}", self.database.display()),
            format!("task id: {}", self.task_id),
        ];
        if self.command == Command::Handoffs {
            // A read-only listing needs none of the execution settings, and
            // printing them would suggest the command might launch something.
            lines.push("read-only listing; nothing is launched and nothing is written".into());
            return lines;
        } else if self.command == Command::Run {
            lines.push(format!("verification checks: {}", self.checks.join(", ")));
        } else {
            // On a resume the persisted criteria are authoritative, so what is
            // reported is whether any override was offered, not a command list that
            // might be mistaken for the real contract.
            lines.push(format!(
                "verification checks: {}",
                if self.checks.is_empty() {
                    "the task's persisted criteria (none supplied to override them)".to_string()
                } else {
                    format!(
                        "supplied, to be matched against the persisted criteria: {}",
                        self.checks.join(", ")
                    )
                }
            ));
        }
        lines.extend([
            format!("agent binary: {}", self.agent_binary),
            format!(
                "permission mode: {}",
                self.permission_mode.as_deref().unwrap_or(
                    "(the harness's own default — Orqyn will not choose a permissive one for you)"
                )
            ),
            format!("agent timeout: {}s", self.agent_timeout.as_secs()),
            format!(
                "external memory: {}",
                if self.memory { "enabled" } else { "disabled" }
            ),
        ]);
        if self.command == Command::Resume {
            lines.push(format!(
                "authorised remediation: {}",
                match self.remediate {
                    None => "(none — a resume will not retry a task without being told to)",
                    Some(choice) => choice_word(choice),
                }
            ));
        } else {
            lines.push(format!("dry run: {}", self.dry_run));
        }
        lines
    }
}

/// Where the binary reads optional configuration from.
///
/// A trait so tests can supply values without touching the real environment,
/// which is process-global and would make tests order-dependent.
pub trait EnvSource {
    /// The value of `name`, if set and non-empty.
    fn get(&self, name: &str) -> Option<String>;
}

/// Reads the process environment.
pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    }
}

/// An environment supplied by a test.
///
/// Only the test target constructs this — the binary reads [`ProcessEnv`] — but it
/// lives here beside [`EnvSource`] because that is the seam it exists to provide.
#[allow(dead_code)]
pub struct MapEnv(pub Vec<(String, String)>);

impl EnvSource for MapEnv {
    fn get(&self, name: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
    }
}

/// The word a remediation was written as, for diagnostics.
fn choice_word(choice: Remediation) -> &'static str {
    match choice {
        Remediation::Retry => "retry",
        Remediation::Rework => "rework",
        Remediation::Cancel => "cancel",
    }
}

/// Parse a whole number of seconds, refusing zero.
fn parse_seconds(value: String, flag: &'static str) -> Result<u64, ConfigError> {
    let seconds: u64 = value.parse().map_err(|_| ConfigError::OutOfRange {
        flag,
        expected: "a whole number of seconds",
    })?;
    if seconds == 0 {
        return Err(ConfigError::OutOfRange {
            flag,
            expected: "greater than zero seconds",
        });
    }
    Ok(seconds)
}

/// Derive a stable task id from a description.
///
/// Deterministic so that re-running the same command addresses the same task
/// rather than creating a second one. Ids are uppercased, non-alphanumerics
/// collapse to `-`, and the result is capped so a long sentence cannot produce an
/// unwieldy identifier.
pub fn derive_task_id(task: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for ch in task.chars() {
        if out.chars().count() >= 48 {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_uppercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "TASK".to_string()
    } else {
        out
    }
}

/// Whether a bare name resolves on `PATH`, honouring `PATHEXT` on Windows.
fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.to_ascii_lowercase())
            .collect()
    } else {
        Vec::new()
    };
    for dir in std::env::split_paths(&path) {
        if dir.join(name).is_file() {
            return true;
        }
        for ext in &extensions {
            if dir.join(format!("{name}{ext}")).is_file() {
                return true;
            }
        }
    }
    false
}
