//! `odin approve` / `odin reject`: record a human decision on a paused approval gate, then
//! resume the run. Both need the workflow file so the engine can continue execution. `reject
//! --rerun` additionally starts a fresh run carrying the note as the `feedback` param.

use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr as _;
use std::sync::Arc;

use anyhow::Context as _;
use odin_core::{Decision, Engine, EngineBuilder, RunId, RunStatus, SqliteStore, Workflow};
use tokio::runtime::Runtime;

/// Arguments shared by `approve` and `reject`.
pub(crate) struct ApprovalArgs {
    pub run_id: String,
    pub workflow: PathBuf,
    pub recipes_dir: Option<PathBuf>,
    pub by: String,
    pub note: Option<String>,
    pub rerun: bool,
    pub repo: Option<PathBuf>,
    pub db: Option<PathBuf>,
    /// Emit the resulting `RunSummary` (or `RerunOutcome`) as JSON on stdout instead of the
    /// human summary, so an approval bot can parse the outcome.
    pub json: bool,
}

/// Approves the run's pending gate and resumes it.
pub(crate) fn approve(args: ApprovalArgs) -> anyhow::Result<ExitCode> {
    if args.rerun {
        anyhow::bail!("--rerun only applies to `reject` (there is nothing to redo on approve)");
    }
    submit(Decision::Approved, args)
}

/// Rejects the run's pending gate (failing it with the `--note` feedback) and resumes; with
/// `--rerun`, also starts a fresh run of the workflow carrying that feedback.
pub(crate) fn reject(args: ApprovalArgs) -> anyhow::Result<ExitCode> {
    let note = args.note.as_deref().unwrap_or("").trim().to_owned();
    if note.is_empty() {
        anyhow::bail!("--note is required when rejecting (the feedback to act on)");
    }
    if args.rerun {
        return reject_rerun(note, args);
    }
    submit(Decision::Rejected, args)
}

/// The engine + runtime + parsed inputs needed to talk to a paused run.
struct Resolved {
    run_id: RunId,
    workflow: Workflow,
    engine: Arc<dyn Engine>,
    runtime: Runtime,
}

fn resolve(args: &ApprovalArgs) -> anyhow::Result<Resolved> {
    let run_id = RunId::from_str(&args.run_id)
        .map_err(|_| anyhow::anyhow!("invalid run id {:?}", args.run_id))?;
    // Accept a recipe name as well as a file path for `--workflow`, matching `odin run` — so a run
    // started by name (`odin run gated-deploy`) can be approved by name too.
    let file = crate::catalog::resolve_arg(&args.workflow, args.recipes_dir.as_deref())?;
    let src =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let workflow = Workflow::from_yaml_str(&src)
        .map_err(|e| anyhow::anyhow!("{}: parse error\n  {e}", file.display()))?;

    let repo = args.repo.clone().unwrap_or_else(|| PathBuf::from("."));
    let db = args
        .db
        .clone()
        .unwrap_or_else(|| repo.join(".odin").join("state.db"));
    let store = SqliteStore::open(&db).context("opening the run state database")?;
    let mut builder = EngineBuilder::new().repo(&repo).store(Arc::new(store));
    // Continue spooling step logs when the run resumes (or reruns), beside `odin run`'s logs.
    if let Some(logs) = crate::cmd::logs_dir_for(&db) {
        builder = builder.logs_dir(logs);
    }
    let engine = builder.build()?;
    let runtime = Runtime::new().context("starting the async runtime")?;
    Ok(Resolved {
        run_id,
        workflow,
        engine,
        runtime,
    })
}

fn submit(decision: Decision, args: ApprovalArgs) -> anyhow::Result<ExitCode> {
    let json = args.json;
    let r = resolve(&args)?;
    let result = r.runtime.block_on(r.engine.submit_approval(
        r.run_id,
        decision,
        args.by,
        args.note,
        std::slice::from_ref(&r.workflow),
    ));
    match result {
        Ok(Some(summary)) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                crate::cmd::run::print_summary(&summary);
            }
            Ok(exit_for(summary.status))
        }
        Ok(None) => fail(json, &format!("no run {} found in the store", r.run_id)),
        Err(e) => fail(json, &e.to_string()),
    }
}

/// The error/not-found arm: a `{ok:false, phase:"error", error}` envelope on stdout under
/// `--json` (so a bot never gets empty stdout), else a human line on stderr. Exit `2`.
fn fail(json: bool, error: &str) -> anyhow::Result<ExitCode> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&crate::cmd::validate::json_error_envelope(
                "error", error
            ))?
        );
    } else {
        eprintln!("✗ {error}");
    }
    Ok(ExitCode::from(2))
}

fn reject_rerun(note: String, args: ApprovalArgs) -> anyhow::Result<ExitCode> {
    let json = args.json;
    let r = resolve(&args)?;
    let result = r.runtime.block_on(r.engine.reject_and_rerun(
        r.run_id,
        args.by,
        note,
        std::slice::from_ref(&r.workflow),
    ));
    match result {
        Ok(Some(outcome)) => {
            if json {
                // `{ "rejected": RunSummary, "rerun": RunSummary }`.
                println!("{}", serde_json::to_string_pretty(&outcome)?);
            } else {
                crate::cmd::run::print_summary(&outcome.rejected);
                println!("↻ rerunning as {} with your feedback", outcome.rerun.run_id);
                crate::cmd::run::print_summary(&outcome.rerun);
            }
            // The rerun's outcome is the actionable one (it may pause again at the gate).
            Ok(exit_for(outcome.rerun.status))
        }
        Ok(None) => fail(json, &format!("no run {} found in the store", r.run_id)),
        Err(e) => fail(json, &e.to_string()),
    }
}

/// A succeeded or paused-again run exits `0`; any other terminal status exits `1`.
fn exit_for(status: RunStatus) -> ExitCode {
    match status {
        RunStatus::Succeeded | RunStatus::AwaitingApproval => ExitCode::SUCCESS,
        _ => ExitCode::from(1),
    }
}
