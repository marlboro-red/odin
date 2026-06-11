//! `odin list` / `show` / `logs`: read durable runs back from the store.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context as _;
use odin_core::traits::{RunEvent, RunState};
use odin_core::{ArtifactName, RunId, RunStatus, SqliteStore, StepStatus, Store};

/// Resolves the state DB path (`--db`, else `<repo>/.odin/state.db`) and opens it if it
/// exists. `Ok(None)` means there is no database yet (printed for the caller to handle).
fn open(repo: Option<PathBuf>, db: Option<PathBuf>) -> anyhow::Result<Option<SqliteStore>> {
    let path = db.unwrap_or_else(|| {
        repo.unwrap_or_else(|| PathBuf::from("."))
            .join(".odin")
            .join("state.db")
    });
    if !path.exists() {
        eprintln!(
            "no run state database at {} — a run is recorded here once you run a `durable` \
             workflow without `--no-store` (the provider-free quickstart is a stateless one-shot \
             and won't appear).",
            path.display()
        );
        return Ok(None);
    }
    let store = SqliteStore::open(&path).with_context(|| format!("opening {}", path.display()))?;
    Ok(Some(store))
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Runtime::new().context("starting the async runtime")
}

fn status_str(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Pending => "pending",
        RunStatus::Running => "running",
        // Canonical wire tag (underscore), matching the serde `RunStatus` representation used
        // everywhere else (`view.rs`, `odin status`) so `inspect --json` doesn't drift.
        RunStatus::AwaitingApproval => "awaiting_approval",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
        _ => "unknown",
    }
}

/// `odin list` — the most recent runs.
pub(crate) fn list(
    repo: Option<PathBuf>,
    db: Option<PathBuf>,
    limit: usize,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let Some(store) = open(repo, db)? else {
        if json {
            println!("[]");
        }
        return Ok(ExitCode::SUCCESS);
    };
    runtime()?.block_on(async {
        let runs = store.recent(limit).await?;
        if json {
            let summaries: Vec<_> = runs
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "run_id": r.run_id.to_string(),
                        "workflow": r.workflow.as_str(),
                        "status": status_str(r.status),
                        "updated_at": r.updated_at.to_rfc3339(),
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&summaries)?);
        } else if runs.is_empty() {
            println!(
                "no runs recorded yet (a `durable` run not started with `--no-store` appears here)"
            );
        } else {
            for r in &runs {
                println!(
                    "{}  {:<10}  {:<20}  {}",
                    r.run_id,
                    status_str(r.status),
                    r.workflow,
                    r.updated_at.to_rfc3339()
                );
            }
        }
        Ok(ExitCode::SUCCESS)
    })
}

/// `odin show <run_id>` — a run's details.
pub(crate) fn show(
    run_id: &str,
    repo: Option<PathBuf>,
    db: Option<PathBuf>,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let run_id: RunId = run_id.parse().context("invalid run id (expected a UUID)")?;
    let Some(store) = open(repo, db)? else {
        if json {
            println!("null");
        }
        return Ok(ExitCode::from(1));
    };
    runtime()?.block_on(async {
        match store.load_run(run_id).await? {
            None => {
                if json {
                    println!("null");
                }
                eprintln!("no run {run_id}");
                Ok(ExitCode::from(1))
            }
            Some(state) => {
                if json {
                    println!("{}", serde_json::to_string_pretty(&state)?);
                } else {
                    print_run(&state);
                }
                Ok(ExitCode::SUCCESS)
            }
        }
    })
}

/// `odin logs <run_id>` — a run's event log.
pub(crate) fn logs(
    run_id: &str,
    repo: Option<PathBuf>,
    db: Option<PathBuf>,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let run_id: RunId = run_id.parse().context("invalid run id (expected a UUID)")?;
    let Some(store) = open(repo, db)? else {
        if json {
            println!("[]");
        }
        return Ok(ExitCode::from(1));
    };
    runtime()?.block_on(async {
        let events: Vec<RunEvent> = store.events(run_id).await?;
        if json {
            println!("{}", serde_json::to_string_pretty(&events)?);
        } else if events.is_empty() {
            println!("no events for {run_id}");
        } else {
            for event in &events {
                println!("{}", serde_json::to_string(event)?);
            }
        }
        Ok(ExitCode::SUCCESS)
    })
}

fn print_run(state: &RunState) {
    println!("run      {}", state.run_id);
    println!("workflow {}", state.workflow);
    println!("status   {}", status_str(state.status));
    if let Some(error) = &state.error {
        println!("error    {error}");
    }
    println!("created  {}", state.created_at.to_rfc3339());
    println!("updated  {}", state.updated_at.to_rfc3339());
    if !state.steps.is_empty() {
        println!("steps:");
        for (id, step) in &state.steps {
            let exit = step
                .exit_code
                .map_or(String::new(), |c| format!(" exit {c}"));
            let attempts = if step.attempts > 1 {
                format!(" ({} attempts)", step.attempts)
            } else {
                String::new()
            };
            println!("  {id:<12} {:?}{exit}{attempts}", step.status);
            if step.status == StepStatus::Failed {
                if let Some(reason) = step.error.as_deref().and_then(|e| e.lines().next()) {
                    println!("  {:<12}   ↳ {reason}", "");
                }
            }
        }
    }
    if state.artifacts.contains_key(&ArtifactName::new("DIFF")) {
        println!("diff     captured");
    }
}
