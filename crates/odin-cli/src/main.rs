//! `odin` — the command-line runner for the Odin workflow engine.
//!
//! Subcommands: `validate` (parse + check a workflow), `run` (execute one), and the
//! read commands `list` / `show` / `logs` over the durable run store.

mod cmd;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Orchestrate autonomous coding-agent CLIs with durable, configurable workflows.
#[derive(Parser)]
#[command(name = "odin", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Parse and validate a workflow file, reporting all diagnostics.
    Validate {
        /// Path to the workflow YAML file.
        file: PathBuf,
        /// Emit the diagnostics report as JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Run a workflow to completion.
    Run {
        /// Path to the workflow YAML file.
        file: PathBuf,
        /// A typed input parameter as `KEY=VALUE` (repeatable). Values parse as JSON if
        /// possible (so `42` / `true` are typed), otherwise as a string.
        #[arg(long = "param", value_name = "KEY=VALUE")]
        param: Vec<String>,
        /// The trigger name to record for this run.
        #[arg(long)]
        trigger: Option<String>,
        /// The git repository to provision workspaces from. Defaults to the current dir.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Path to the run-state SQLite database. Defaults to `<repo>/.odin/state.db`.
        #[arg(long)]
        db: Option<PathBuf>,
        /// Do not persist run state (no durability / resume).
        #[arg(long)]
        no_store: bool,
        /// Emit the run summary as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List the most recent runs from the store.
    List {
        /// The git repository whose `.odin/state.db` to read. Defaults to the current dir.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Path to the run-state SQLite database. Overrides `--repo`.
        #[arg(long)]
        db: Option<PathBuf>,
        /// Maximum number of runs to list.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Emit the listing as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show a run's details.
    Show {
        /// The run id (UUID).
        run_id: String,
        /// The git repository whose `.odin/state.db` to read. Defaults to the current dir.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Path to the run-state SQLite database. Overrides `--repo`.
        #[arg(long)]
        db: Option<PathBuf>,
        /// Emit the full run state as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show a run's event log.
    Logs {
        /// The run id (UUID).
        run_id: String,
        /// The git repository whose `.odin/state.db` to read. Defaults to the current dir.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Path to the run-state SQLite database. Overrides `--repo`.
        #[arg(long)]
        db: Option<PathBuf>,
        /// Emit the events as a JSON array.
        #[arg(long)]
        json: bool,
    },
}

/// Maps a command result to a process exit code, printing any error.
fn finish(result: anyhow::Result<ExitCode>) -> ExitCode {
    result.unwrap_or_else(|e| {
        eprintln!("error: {e:#}");
        ExitCode::from(2)
    })
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Validate { file, json } => match cmd::validate::run(&file, json) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::from(2)
            }
        },
        Command::Run {
            file,
            param,
            trigger,
            repo,
            db,
            no_store,
            json,
        } => {
            let args = cmd::run::RunArgs {
                file,
                params: param,
                trigger,
                repo,
                db,
                no_store,
                json,
            };
            match cmd::run::run(args) {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("error: {e:#}");
                    ExitCode::from(2)
                }
            }
        }
        Command::List {
            repo,
            db,
            limit,
            json,
        } => finish(cmd::inspect::list(repo, db, limit, json)),
        Command::Show {
            run_id,
            repo,
            db,
            json,
        } => finish(cmd::inspect::show(&run_id, repo, db, json)),
        Command::Logs {
            run_id,
            repo,
            db,
            json,
        } => finish(cmd::inspect::logs(&run_id, repo, db, json)),
    }
}
