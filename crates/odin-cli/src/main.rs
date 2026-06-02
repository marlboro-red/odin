//! `odin` — the command-line runner for the Odin workflow engine.
//!
//! `validate` is fully implemented; the execution subcommands (`run`, `list`, `show`,
//! `logs`) are scaffolded and arrive with the engine milestone.

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
    /// Run a workflow (arrives with the execution milestone).
    Run {
        /// Path to the workflow YAML file.
        file: PathBuf,
    },
    /// List runs (arrives with the durable-store milestone).
    List,
    /// Show a run's details (arrives with the durable-store milestone).
    Show {
        /// The run id.
        run_id: String,
    },
    /// Tail a run's logs (arrives with the execution milestone).
    Logs {
        /// The run id.
        run_id: String,
    },
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
        Command::Run { .. } | Command::List | Command::Show { .. } | Command::Logs { .. } => {
            eprintln!(
                "error: this subcommand is not implemented yet (tracked for a later milestone)"
            );
            ExitCode::from(2)
        }
    }
}
