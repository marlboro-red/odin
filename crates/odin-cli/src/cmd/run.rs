//! The `odin run` subcommand: execute a workflow and report the result.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context as _;
use odin_core::{
    EngineBuilder, Error, RunInput, RunStatus, RunSummary, SqliteStore, StepStatus, Workflow,
};

/// Parsed arguments for `odin run`.
pub(crate) struct RunArgs {
    pub file: PathBuf,
    pub params: Vec<String>,
    pub trigger: Option<String>,
    pub repo: Option<PathBuf>,
    pub db: Option<PathBuf>,
    pub no_store: bool,
    pub json: bool,
}

/// Runs the workflow at `args.file`. Exit: `0` succeeded, `1` failed / invalid, `2` parse/IO.
pub(crate) fn run(args: RunArgs) -> anyhow::Result<ExitCode> {
    let src = std::fs::read_to_string(&args.file)
        .with_context(|| format!("reading {}", args.file.display()))?;
    let workflow = match Workflow::from_yaml_str(&src) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("✗ {}: parse error\n  {e}", args.file.display());
            return Ok(ExitCode::from(2));
        }
    };

    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    runtime.block_on(execute(&workflow, args))
}

async fn execute(workflow: &Workflow, args: RunArgs) -> anyhow::Result<ExitCode> {
    let repo = args.repo.clone().unwrap_or_else(|| PathBuf::from("."));
    let mut builder = EngineBuilder::new().repo(&repo);

    if !args.no_store {
        let db = args
            .db
            .clone()
            .unwrap_or_else(|| repo.join(".odin").join("state.db"));
        if let Some(parent) = db.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let store = SqliteStore::open(&db).context("opening the run state database")?;
        builder = builder.store(Arc::new(store));
    }
    let engine = builder.build()?;

    let mut input = RunInput::manual();
    if let Some(trigger) = args.trigger {
        input.trigger = trigger;
    }
    for pair in &args.params {
        let (key, value) = pair
            .split_once('=')
            .with_context(|| format!("--param must be KEY=VALUE, got {pair:?}"))?;
        if key.trim().is_empty() {
            anyhow::bail!("--param key must be non-empty, got {pair:?}");
        }
        input.params.insert(key.to_owned(), parse_value(value));
    }

    match engine.run(workflow, input).await {
        Ok(summary) => {
            if args.json {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                print_summary(&summary);
            }
            // A run paused for approval isn't a failure — it's awaiting input.
            Ok(match summary.status {
                RunStatus::Succeeded | RunStatus::AwaitingApproval => ExitCode::SUCCESS,
                _ => ExitCode::from(1),
            })
        }
        Err(Error::Validation(report)) => {
            for diagnostic in &report.diagnostics {
                eprintln!("{diagnostic}\n");
            }
            eprintln!(
                "✗ {}: {} error(s)",
                args.file.display(),
                report.error_count()
            );
            Ok(ExitCode::from(1))
        }
        Err(e) => {
            eprintln!("✗ run failed: {e}");
            Ok(ExitCode::from(2))
        }
    }
}

/// Parses a `--param` value as JSON if possible (so `42`/`true` are typed), else a string.
fn parse_value(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_owned()))
}

fn glyph(status: StepStatus) -> char {
    match status {
        StepStatus::Passed => '✓',
        StepStatus::Failed => '✗',
        StepStatus::Skipped => '⊘',
        StepStatus::AwaitingApproval => '⏸',
        _ => '·',
    }
}

pub(crate) fn print_summary(summary: &RunSummary) {
    let status = match summary.status {
        RunStatus::Succeeded => "succeeded",
        RunStatus::AwaitingApproval => "awaiting approval",
        RunStatus::Cancelled => "cancelled",
        _ => "failed",
    };
    println!("Run {} — {status}", summary.run_id);
    for step in &summary.steps {
        let exit = step
            .exit_code
            .map_or(String::new(), |c| format!(" (exit {c})"));
        println!("  {} {}{exit}", glyph(step.status), step.id);
        // Surface why a step failed (first line of the recorded reason) right under it.
        if step.status == StepStatus::Failed {
            if let Some(reason) = step.error.as_deref().and_then(|e| e.lines().next()) {
                println!("      ↳ {reason}");
            }
        }
        // For a paused gate, show its message and how to act on it.
        if step.status == StepStatus::AwaitingApproval {
            if let Some(msg) = step.outputs.get("message").and_then(|v| v.as_str()) {
                println!("      ↳ {msg}");
            }
            println!(
                "      ↳ approve: `odin approve {} --workflow <file> --by <you>`",
                summary.run_id
            );
        }
    }
    println!(
        "usage: {} in / {} out tokens, ${:.4}",
        summary.usage.input_tokens,
        summary.usage.output_tokens,
        summary.usage.cost_usd()
    );
    if let Some(error) = &summary.error {
        println!("error: {error}");
    }
    if !summary.side_effects.is_empty() {
        println!("side-effects: {}", summary.side_effects.len());
    }
}
