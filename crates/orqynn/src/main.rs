//! `orqynn` — the command-line entry point.
//!
//! One command, one task, one lifecycle. Everything it does is a call into
//! `director-app`; this file is the shell that parses arguments, opens the
//! store, decides which substrates are in play, and turns the result into an
//! exit code.
//!
//! ```text
//! orqynn run --repo <path> --task <description> --check <command> [...]
//! ```
//!
//! `--dry-run` reports exactly what would happen and starts nothing.

mod config;
mod lifecycle;

use std::process::ExitCode as ProcessExitCode;

use lifecycle::{CliMemory, ExitCode, RunError};

use config::{ProcessEnv, RunArgs};

const USAGE: &str = "\
orqynn — drive one task through Orqyn's loop, or recover one from persisted state

USAGE:
    orqynn run    --repo <path> --task <description> --check <command>... [options]
    orqynn resume --repo <path> --task-id <id> [--check <command>]... [--remediate retry]

REQUIRED (for a real run):
    --repo <path>              git repository to observe and work in
    --task <description>       the task, in your words
    --check <command>          a command VERIFY runs; repeat for several.
                               At least one is required unless --dry-run.

REQUIRED (for a resume):
    --repo <path>              the same repository the task was planned in
    --task-id <id>             the task to recover; it will not create a new one
                               --check is optional: the persisted criteria are used

OPTIONS:
    --task-id <id>             task identifier; derived from --task when absent
    --db <path>                SQLite state; defaults to <repo>/.orqynn/orqynn.db
    --agent-binary <path>      the agent executable; default `claude` from PATH
    --permission-mode <mode>   passed to the harness; Orqyn picks none for you
    --model <model>            passed to the harness
    --add-dir <path>           an extra directory the agent may reach; repeatable
    --agent-timeout <seconds>  wall-clock limit for the agent; default 1800
    --memory-timeout <seconds> limit on memory retrieval; default 5
    --memory                   enable the ai-memory substrate
    --dry-run                  report what would happen; start nothing
    --remediate <r|r|c>        resume only: retry | rework | cancel. Resume will
                               never re-drive a task without being told to.

ENVIRONMENT:
    ORQYNN_REPO, ORQYNN_TASK, ORQYNN_DB, ORQYNN_AGENT_BINARY,
    ORQYNN_AGENT_PERMISSION_MODE, ORQYNN_AGENT_MODEL
    AI_MEMORY_BINARY, AI_MEMORY_DATA_DIR, AI_MEMORY_WORKSPACE, AI_MEMORY_PROJECT

EXIT CODES:
    0 verification passed      5 a check failed (the task is NOT done)
    2 bad configuration       6 could not judge the result
    3 a loop step failed       7 dry run
    4 the agent did not run    8 resume refused (unsafe state; nothing ran)
";

fn main() -> ProcessExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let mut args = match RunArgs::from_env_and_args(argv, &ProcessEnv) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("orqynn: {err}\n\n{USAGE}");
            return status(ExitCode::Configuration);
        }
    };

    if let Err(err) = args.resolve() {
        eprintln!("orqynn: {err}\n\n{USAGE}");
        return status(ExitCode::Configuration);
    }

    if args.dry_run {
        // Nothing is opened, nothing is spawned. The report is built from the
        // same resolved arguments the real run would use.
        for line in lifecycle::dry_run_report(&args) {
            println!("{line}");
        }
        return status(ExitCode::DryRun);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|err| {
            eprintln!("orqynn: could not start a runtime: {err}");
            std::process::exit(ExitCode::Configuration.status());
        });

    runtime.block_on(run(&args))
}

async fn run(args: &RunArgs) -> ProcessExitCode {
    // 1. Describe what is about to happen, before anything happens. A user who
    //    cannot see the target should not have started it.
    println!("orqynn {}", args.command.as_str());
    for line in args.provenance() {
        println!("  {line}");
    }

    // 2. Reach the memory substrate, or refuse before starting.
    let memory = match lifecycle::connect_memory(args).await {
        Ok(memory) => memory,
        Err(err) => return fail(&err),
    };
    let memory = match memory {
        Some(adapter) => CliMemory::External(adapter),
        None => CliMemory::Local(director_adapters::InMemoryProvider::new()),
    };

    // 3. Open the store and the git service.
    let runtime = match lifecycle::open_runtime(args).await {
        Ok(runtime) => runtime,
        Err(err) => return fail(&err),
    };

    // 3b. Say whether this database has been used before. An interrupted run
    //     leaves its records here, and "resuming" means re-running the same
    //     command — the store, not the CLI, decides what that finds.
    let known = lifecycle::project_is_known(&runtime.store).await;
    let database_note = if known {
        "  resuming an existing database"
    } else if args.command == config::Command::Resume {
        "  this database holds no project; there may be nothing to resume"
    } else {
        "  fresh database"
    };
    println!("{database_note}");

    // 4. Drive the lifecycle. The driver is the configured agent binary; nothing
    //    about it is faked here.
    let driver = lifecycle::claude_driver(args);
    let executor = director_adapters::executor::LocalExecutor::default();

    let lines: Vec<String>;
    let exit_code: ExitCode;

    match args.command {
        config::Command::Run => {
            let report = match lifecycle::run_lifecycle(
                &runtime,
                &memory,
                &driver,
                &executor,
                args,
                chrono::Utc::now(),
            )
            .await
            {
                Ok(report) => report,
                Err(err) => return fail(&err),
            };
            lines = report.lines;
            exit_code = report.exit_code;
        }
        config::Command::Resume => {
            let report = match lifecycle::resume_lifecycle(
                &runtime,
                &memory,
                &driver,
                &executor,
                args,
                chrono::Utc::now(),
            )
            .await
            {
                Ok(report) => report,
                Err(err) => return fail(&err),
            };
            lines = report.lines;
            exit_code = report.exit_code;
            for line in &lines {
                println!("{line}");
            }
            println!();
            println!(
                "decision: {}",
                match report.action {
                    lifecycle::ResumeAction::Refused => "refused; nothing was launched",
                    lifecycle::ResumeAction::Reverified => {
                        "re-verified against the current working tree; no agent was launched"
                    }
                    lifecycle::ResumeAction::Redriven => {
                        "an authorised remediation was applied and the task was driven"
                    }
                }
            );
            println!("store: {}", args.database.display());
            return status(exit_code);
        }
        config::Command::Handoffs => {
            // Read-only: this path opens the store, reads, prints, and returns.
            // It starts no agent and writes nothing.
            let listed = match lifecycle::list_handoffs(&runtime, args).await {
                Ok(lines) => lines,
                Err(err) => return fail(&err),
            };
            for line in &listed {
                println!("{line}");
            }
            println!();
            println!("store: {}", args.database.display());
            println!("(read-only: no record was created, changed, or accepted)");
            return status(ExitCode::Success);
        }
    }

    println!();
    for line in &lines {
        println!("{line}");
    }
    println!();
    println!("state: {}", runtime.state_dir.display());
    println!("store: {}", args.database.display());
    if matches!(memory, CliMemory::External(_)) {
        println!("memory: external substrate in use");
    } else {
        println!("memory: disabled (checkpoints stay in SQLite)");
    }

    status(exit_code)
}

fn fail(err: &RunError) -> ProcessExitCode {
    eprintln!("orqynn: {err}");
    let code = match err {
        RunError::Config(_) => ExitCode::Configuration,
        RunError::Orchestration { .. } | RunError::Setup(_) => ExitCode::Orchestration,
    };
    status(code)
}

fn status(code: ExitCode) -> ProcessExitCode {
    ProcessExitCode::from(code.status() as u8)
}
