//! `odin` — the command-line runner for the Odin workflow engine.
//!
//! Subcommands: `validate` (parse + check a workflow), `run` (execute one), the
//! read commands `list` / `show` / `logs` over the durable run store, and `recipe`
//! (manage the by-name workflow catalog).

mod catalog;
mod cmd;
mod scaffold;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Orchestrate autonomous coding-agent CLIs with durable, configurable workflows.
#[derive(Parser)]
#[command(name = "odin", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Diagnostic-log format (the level is `$ODIN_LOG`/`$RUST_LOG`, default `info`; e.g.
    /// `ODIN_LOG=debug odin run …` to see per-step engine spans). Command *output*
    /// (summaries, tables, `--json`) always goes to stdout regardless.
    #[arg(long, value_name = "FORMAT", default_value = "text", value_parser = ["text", "json"], global = true)]
    log_format: String,
}

#[derive(Subcommand)]
enum Command {
    /// Parse and validate a workflow file, reporting all diagnostics.
    Validate {
        /// The workflow: a path to a YAML file, or a recipe name in the catalog.
        file: PathBuf,
        /// Override the recipe catalog directory used to resolve a name.
        #[arg(long, value_name = "DIR")]
        recipes_dir: Option<PathBuf>,
        /// Emit the diagnostics report as JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Run a workflow to completion.
    Run {
        /// The workflow: a path to a YAML file, or a recipe name in the catalog.
        file: PathBuf,
        /// Override the recipe catalog directory used to resolve a name.
        #[arg(long, value_name = "DIR")]
        recipes_dir: Option<PathBuf>,
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
    /// Approve a run paused at an `approval` gate and resume it.
    Approve(ApprovalCmd),
    /// Reject a run paused at an `approval` gate (failing the gate with `--note` feedback).
    Reject(ApprovalCmd),
    /// Delete old/excess terminal runs from the store (never touches in-flight or awaiting runs).
    Prune(PruneCmd),
    /// Manage the workflow recipe catalog (run/validate workflows by name).
    Recipe(RecipeCmd),
    /// At-a-glance status of recent runs (counts + steps); `--watch` live-refreshes.
    Status {
        /// The git repository whose `.odin/state.db` to read. Defaults to the current dir.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Path to the run-state SQLite database. Overrides `--repo`.
        #[arg(long)]
        db: Option<PathBuf>,
        /// Maximum number of runs to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Live-refresh every 2s until ctrl-c.
        #[arg(long)]
        watch: bool,
        /// Emit the runs as JSON (the same shape as the daemon's `/api/runs`).
        #[arg(long)]
        json: bool,
    },
}

/// Arguments for `prune`. Requires at least one of `--older-than` / `--keep-last`.
#[derive(clap::Args)]
struct PruneCmd {
    /// Prune terminal runs last updated longer ago than this (e.g. `90d`, `12h`, `2w`).
    #[arg(long, value_name = "DURATION")]
    older_than: Option<String>,
    /// Keep at most this many terminal runs per workflow (newest first); prune the rest.
    #[arg(long, value_name = "N")]
    keep_last: Option<u32>,
    /// Restrict pruning to a single workflow (by name).
    #[arg(long, value_name = "NAME")]
    workflow: Option<String>,
    /// Preview what would be pruned and delete nothing.
    #[arg(long)]
    dry_run: bool,
    /// Skip the interactive confirmation (required to prune non-interactively).
    #[arg(long)]
    yes: bool,
    /// Emit the prune report as JSON.
    #[arg(long)]
    json: bool,
    /// The git repository whose `.odin/state.db` to use. Defaults to the current dir.
    #[arg(long)]
    repo: Option<PathBuf>,
    /// Path to the run-state SQLite database. Overrides `--repo`.
    #[arg(long)]
    db: Option<PathBuf>,
}

impl From<PruneCmd> for cmd::prune::PruneArgs {
    fn from(c: PruneCmd) -> Self {
        Self {
            older_than: c.older_than,
            keep_last: c.keep_last,
            workflow: c.workflow,
            dry_run: c.dry_run,
            yes: c.yes,
            json: c.json,
            repo: c.repo,
            db: c.db,
        }
    }
}

/// Shared arguments for `approve` / `reject`.
#[derive(clap::Args)]
struct ApprovalCmd {
    /// The run id (UUID) of the paused run.
    run_id: String,
    /// The workflow file the run was started from (needed to resume).
    #[arg(long)]
    workflow: PathBuf,
    /// Who is approving/rejecting (recorded for the audit trail).
    #[arg(long, default_value = "cli")]
    by: String,
    /// A note: the feedback to act on. **Required** when rejecting.
    #[arg(long)]
    note: Option<String>,
    /// (reject only) After failing the gate, start a fresh run of the workflow carrying the
    /// `--note` as the `feedback` param, so the agent can address it and try again.
    #[arg(long)]
    rerun: bool,
    /// The git repository whose `.odin/state.db` to use. Defaults to the current dir.
    #[arg(long)]
    repo: Option<PathBuf>,
    /// Path to the run-state SQLite database. Overrides `--repo`.
    #[arg(long)]
    db: Option<PathBuf>,
}

impl From<ApprovalCmd> for cmd::approval::ApprovalArgs {
    fn from(c: ApprovalCmd) -> Self {
        Self {
            run_id: c.run_id,
            workflow: c.workflow,
            by: c.by,
            note: c.note,
            rerun: c.rerun,
            repo: c.repo,
            db: c.db,
        }
    }
}

/// The `recipe` subcommand group.
#[derive(clap::Args)]
struct RecipeCmd {
    #[command(subcommand)]
    command: RecipeSub,
}

#[derive(Subcommand)]
enum RecipeSub {
    /// List the recipes in the catalog (name + description + tags).
    List {
        /// Only list recipes carrying this tag (case-insensitive).
        #[arg(long, value_name = "TAG")]
        tag: Option<String>,
        /// Override the catalog directory (else `$ODIN_RECIPES_DIR`, else the platform default).
        #[arg(long, value_name = "DIR")]
        recipes_dir: Option<PathBuf>,
        /// Emit the listing as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Scaffold a new workflow file from an existing recipe, bundled starter, or file.
    ///
    /// If the source declares a `# odin:template` header, fill its `@@VAR@@` placeholders with
    /// `--set name=value` (defaults apply for the rest).
    New {
        /// The name for the new recipe (becomes its `name:` and the default filename stem).
        name: String,
        /// The source to copy from: a recipe name, a bundled starter name, or a file path.
        #[arg(long, value_name = "SOURCE")]
        from: String,
        /// Fill a template variable (repeatable): `--set key=value`.
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// Write here — a `.yaml`/`.yml` file, or a directory. Default: `./<name>.yaml`.
        #[arg(long, short = 'o', value_name = "PATH", conflicts_with_all = ["catalog", "stdout"])]
        out: Option<PathBuf>,
        /// Install into the recipe catalog as `<name>` (then `odin run <name>`).
        #[arg(long, conflicts_with = "stdout")]
        catalog: bool,
        /// Print the rendered workflow to stdout instead of writing a file.
        #[arg(long)]
        stdout: bool,
        /// Override the catalog directory used to resolve `--from` (and `--catalog`).
        #[arg(long, value_name = "DIR")]
        recipes_dir: Option<PathBuf>,
        /// Overwrite the destination if it already exists.
        #[arg(long)]
        force: bool,
    },
    /// Seed the catalog with the bundled starter recipes.
    Init {
        /// Override the catalog directory.
        #[arg(long, value_name = "DIR")]
        recipes_dir: Option<PathBuf>,
        /// Overwrite recipes that already exist (default: keep them).
        #[arg(long)]
        force: bool,
    },
    /// Copy a workflow file into the catalog as a recipe.
    Add {
        /// Path to the workflow YAML file to add.
        file: PathBuf,
        /// The recipe name to store it under (default: the file's stem).
        #[arg(long = "as", value_name = "NAME")]
        as_name: Option<String>,
        /// Override the catalog directory.
        #[arg(long, value_name = "DIR")]
        recipes_dir: Option<PathBuf>,
        /// Overwrite an existing recipe of the same name.
        #[arg(long)]
        force: bool,
    },
    /// Print a recipe's workflow YAML.
    Show {
        /// The recipe name (its filename stem in the catalog).
        name: String,
        /// Override the catalog directory.
        #[arg(long, value_name = "DIR")]
        recipes_dir: Option<PathBuf>,
    },
    /// Print the filesystem path of a recipe (for scripting).
    Path {
        /// The recipe name (its filename stem in the catalog).
        name: String,
        /// Override the catalog directory.
        #[arg(long, value_name = "DIR")]
        recipes_dir: Option<PathBuf>,
    },
}

/// Dispatches an `odin recipe <SUBCOMMAND>` to its handler.
fn dispatch_recipe(sub: RecipeSub) -> anyhow::Result<ExitCode> {
    match sub {
        RecipeSub::List {
            tag,
            recipes_dir,
            json,
        } => cmd::recipe::list(recipes_dir.as_deref(), tag.as_deref(), json),
        RecipeSub::New {
            name,
            from,
            set,
            out,
            catalog,
            stdout,
            recipes_dir,
            force,
        } => cmd::recipe::new(&cmd::recipe::NewArgs {
            name,
            from,
            set,
            out,
            catalog,
            stdout,
            recipes_dir,
            force,
        }),
        RecipeSub::Init { recipes_dir, force } => cmd::recipe::init(recipes_dir.as_deref(), force),
        RecipeSub::Add {
            file,
            as_name,
            recipes_dir,
            force,
        } => cmd::recipe::add(&file, as_name.as_deref(), force, recipes_dir.as_deref()),
        RecipeSub::Show { name, recipes_dir } => cmd::recipe::show(&name, recipes_dir.as_deref()),
        RecipeSub::Path { name, recipes_dir } => cmd::recipe::path(&name, recipes_dir.as_deref()),
    }
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
    // Structured diagnostics (engine spans/events) to stderr; command output stays on stdout.
    // The CLI is one-shot, so no OTLP exporter — that's the daemon's long-running concern.
    let _telemetry = odin_core::telemetry::init(&odin_core::telemetry::Options {
        format: cli.log_format.parse().unwrap_or_default(),
        otlp_endpoint: None,
    });
    match cli.command {
        Command::Validate {
            file,
            recipes_dir,
            json,
        } => match cmd::validate::run(&file, recipes_dir.as_deref(), json) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::from(2)
            }
        },
        Command::Run {
            file,
            recipes_dir,
            param,
            trigger,
            repo,
            db,
            no_store,
            json,
        } => {
            let args = cmd::run::RunArgs {
                file,
                recipes_dir,
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
        Command::Approve(c) => finish(cmd::approval::approve(c.into())),
        Command::Reject(c) => finish(cmd::approval::reject(c.into())),
        Command::Prune(c) => finish(cmd::prune::run(c.into())),
        Command::Recipe(c) => finish(dispatch_recipe(c.command)),
        Command::Status {
            repo,
            db,
            limit,
            watch,
            json,
        } => finish(cmd::status::run(cmd::status::StatusArgs {
            repo,
            db,
            limit,
            watch,
            json,
        })),
    }
}
