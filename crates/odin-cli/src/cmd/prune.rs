//! `odin prune`: delete old/excess **terminal** runs from the store (and reclaim their git
//! snapshot refs). Never touches a non-terminal run. Requires an explicit age or count limit and
//! — unless `--yes` or `--dry-run` — confirms on a TTY first (auto-declining when non-interactive,
//! so an unattended `odin prune` without `--yes` is a no-op).

use std::io::{IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context as _;
use odin_core::ir::HumanDuration;
use odin_core::{EngineBuilder, PrunePolicy, PruneReport, SqliteStore, WorkflowId};

pub(crate) struct PruneArgs {
    pub older_than: Option<String>,
    pub keep_last: Option<u32>,
    pub workflow: Option<String>,
    pub dry_run: bool,
    pub yes: bool,
    pub json: bool,
    pub repo: Option<PathBuf>,
    pub db: Option<PathBuf>,
}

pub(crate) fn run(args: PruneArgs) -> anyhow::Result<ExitCode> {
    let policy = build_policy(&args)?;
    if policy.is_noop() {
        anyhow::bail!(
            "refusing to prune with no age or count limit (use --older-than and/or --keep-last)"
        );
    }

    let repo = args.repo.unwrap_or_else(|| PathBuf::from("."));
    let db = args
        .db
        .unwrap_or_else(|| repo.join(".odin").join("state.db"));
    let store = SqliteStore::open(&db).context("opening the run state database")?;
    let engine = EngineBuilder::new()
        .repo(&repo)
        .store(Arc::new(store))
        .build()?;
    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;

    // An explicit dry run: report what would go, delete nothing.
    if args.dry_run {
        let report = runtime.block_on(engine.prune(&policy, true))?;
        emit(&report, args.json);
        return Ok(ExitCode::SUCCESS);
    }

    // Otherwise, unless --yes, preview first and confirm (CI-safe: a non-TTY auto-declines).
    if !args.yes {
        let preview = runtime.block_on(engine.prune(&policy, true))?;
        if preview.runs_pruned == 0 {
            println!("Nothing to prune.");
            return Ok(ExitCode::SUCCESS);
        }
        if !confirm(&preview)? {
            println!("Aborted; nothing was pruned.");
            return Ok(ExitCode::SUCCESS);
        }
    }

    let report = runtime.block_on(engine.prune(&policy, false))?;
    emit(&report, args.json);
    Ok(ExitCode::SUCCESS)
}

fn build_policy(args: &PruneArgs) -> anyhow::Result<PrunePolicy> {
    let max_age = match &args.older_than {
        Some(s) => {
            let std = HumanDuration::parse(s)
                .map_err(|e| anyhow::anyhow!("--older-than: {e}"))?
                .as_duration();
            Some(
                chrono::Duration::from_std(std)
                    .map_err(|_| anyhow::anyhow!("--older-than {s:?} is too large"))?,
            )
        }
        None => None,
    };
    Ok(PrunePolicy {
        max_age,
        keep_last: args.keep_last,
        workflow: args.workflow.as_deref().map(WorkflowId::new),
    })
}

/// Prompts `y/N` on a TTY; a non-interactive stdin auto-declines (so unattended runs are safe).
fn confirm(preview: &PruneReport) -> anyhow::Result<bool> {
    print_human(preview);
    if !std::io::stdin().is_terminal() {
        eprintln!("(no TTY; pass --yes to prune non-interactively)");
        return Ok(false);
    }
    print!("Prune these {} run(s)? [y/N] ", preview.runs_pruned);
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn emit(report: &PruneReport, json: bool) {
    if json {
        match serde_json::to_string_pretty(report) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("error: serializing report: {e}"),
        }
    } else {
        print_human(report);
    }
}

fn print_human(report: &PruneReport) {
    let verb = if report.dry_run {
        "Would prune"
    } else {
        "Pruned"
    };
    println!(
        "{verb} {} run(s), {} event(s):",
        report.runs_pruned, report.events_pruned
    );
    for c in &report.per_workflow {
        println!("  {} {} × {}", c.count, c.workflow, c.status);
    }
}
