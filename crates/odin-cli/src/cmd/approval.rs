//! `odin approve` / `odin reject`: record a human decision on a paused approval gate, then
//! resume the run. Both need the workflow file so the engine can continue execution.

use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr as _;
use std::sync::Arc;

use anyhow::Context as _;
use odin_core::{Decision, EngineBuilder, RunId, RunStatus, SqliteStore, Workflow};

/// Arguments shared by `approve` and `reject`.
pub(crate) struct ApprovalArgs {
    pub run_id: String,
    pub workflow: PathBuf,
    pub by: String,
    pub note: Option<String>,
    pub repo: Option<PathBuf>,
    pub db: Option<PathBuf>,
}

/// Approves the run's pending gate and resumes it.
pub(crate) fn approve(args: ApprovalArgs) -> anyhow::Result<ExitCode> {
    submit(Decision::Approved, args)
}

/// Rejects the run's pending gate (failing it with the `--note` feedback) and resumes.
pub(crate) fn reject(args: ApprovalArgs) -> anyhow::Result<ExitCode> {
    if args.note.as_deref().unwrap_or("").trim().is_empty() {
        anyhow::bail!("--note is required when rejecting (the feedback to act on)");
    }
    submit(Decision::Rejected, args)
}

fn submit(decision: Decision, args: ApprovalArgs) -> anyhow::Result<ExitCode> {
    let run_id = RunId::from_str(&args.run_id)
        .map_err(|_| anyhow::anyhow!("invalid run id {:?}", args.run_id))?;
    let src = std::fs::read_to_string(&args.workflow)
        .with_context(|| format!("reading {}", args.workflow.display()))?;
    let workflow = Workflow::from_yaml_str(&src)
        .map_err(|e| anyhow::anyhow!("{}: parse error\n  {e}", args.workflow.display()))?;

    let repo = args.repo.clone().unwrap_or_else(|| PathBuf::from("."));
    let db = args
        .db
        .clone()
        .unwrap_or_else(|| repo.join(".odin").join("state.db"));
    let store = SqliteStore::open(&db).context("opening the run state database")?;
    let engine = EngineBuilder::new()
        .repo(&repo)
        .store(Arc::new(store))
        .build()?;

    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    let result = runtime.block_on(engine.submit_approval(
        run_id,
        decision,
        args.by,
        args.note,
        std::slice::from_ref(&workflow),
    ));
    match result {
        Ok(Some(summary)) => {
            crate::cmd::run::print_summary(&summary);
            Ok(match summary.status {
                RunStatus::Succeeded | RunStatus::AwaitingApproval => ExitCode::SUCCESS,
                _ => ExitCode::from(1),
            })
        }
        Ok(None) => {
            eprintln!("✗ no run {run_id} found in the store");
            Ok(ExitCode::from(2))
        }
        Err(e) => {
            eprintln!("✗ {e}");
            Ok(ExitCode::from(2))
        }
    }
}
