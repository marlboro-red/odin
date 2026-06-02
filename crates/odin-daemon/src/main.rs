//! `odind` — the Odin daemon.
//!
//! A thin runner over [`odin_daemon`]: load a directory of workflow files, build an
//! engine backed by a SQLite store, derive triggers from each workflow's `triggers:`
//! block, and serve until `ctrl-c`. Durable in-flight runs resume on the next start.
//!
//! ```text
//! odind --workflows ./workflows --repo . [--db ./.odin/state.db]
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use odin_core::{EngineBuilder, SqliteStore, Workflow};
use odin_daemon::Daemon;

/// Run Odin workflows from event triggers (cron).
#[derive(Parser)]
#[command(name = "odind", version, about)]
struct Cli {
    /// Directory of workflow files to serve; every `*.yaml` / `*.yml` is loaded.
    #[arg(long, value_name = "DIR")]
    workflows: PathBuf,
    /// Git repository the engine provisions workspaces from.
    #[arg(long, value_name = "DIR", default_value = ".")]
    repo: PathBuf,
    /// SQLite state database. Defaults to `<repo>/.odin/state.db`.
    #[arg(long, value_name = "FILE")]
    db: Option<PathBuf>,
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("odind: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let workflows = load_workflows(&cli.workflows)?;
    if workflows.is_empty() {
        anyhow::bail!("no valid workflows found in {}", cli.workflows.display());
    }
    eprintln!(
        "odind: loaded {} workflow(s) from {}",
        workflows.len(),
        cli.workflows.display()
    );

    let db = cli
        .db
        .clone()
        .unwrap_or_else(|| cli.repo.join(".odin").join("state.db"));
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let store = SqliteStore::open(&db).context("opening the run state database")?;
    let engine = EngineBuilder::new()
        .repo(&cli.repo)
        .store(Arc::new(store))
        .build()?;

    let daemon = Daemon::from_workflows(engine, workflows)?;

    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    runtime.block_on(async move {
        tokio::select! {
            res = daemon.run() => res,
            _ = tokio::signal::ctrl_c() => {
                eprintln!("odind: ctrl-c received, shutting down");
                Ok(())
            }
        }
    })
}

/// Loads every `*.yaml` / `*.yml` workflow in `dir` (sorted for determinism). Files that
/// fail to parse are skipped with a warning rather than aborting startup.
fn load_workflows(dir: &Path) -> anyhow::Result<Vec<Workflow>> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading workflow dir {}", dir.display()))?;
    let mut paths = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| matches!(p.extension().and_then(|s| s.to_str()), Some("yaml" | "yml")))
        .collect::<Vec<_>>();
    paths.sort();

    let mut workflows = Vec::new();
    for path in paths {
        let src = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        match Workflow::from_yaml_str(&src) {
            Ok(workflow) => {
                eprintln!("odind:   • {} ({})", workflow.name.as_str(), path.display());
                workflows.push(workflow);
            }
            Err(e) => eprintln!("odind: skipping {}: {e}", path.display()),
        }
    }
    Ok(workflows)
}
