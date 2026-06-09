//! The `odin run` subcommand: execute a workflow and report the result.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context as _;
use odin_core::{
    EngineBuilder, Error, RunInput, RunStatus, RunSummary, SqliteStore, Step, StepKind, StepStatus,
    Workflow,
};

/// Parsed arguments for `odin run`.
pub(crate) struct RunArgs {
    /// The workflow to run: a file path or a recipe name (see [`crate::catalog::resolve_arg`]).
    pub file: PathBuf,
    pub recipes_dir: Option<PathBuf>,
    pub params: Vec<String>,
    pub trigger: Option<String>,
    pub repo: Option<PathBuf>,
    pub db: Option<PathBuf>,
    pub no_store: bool,
    pub json: bool,
    /// Replace `provider:` steps with a mock that echoes their rendered prompt, so a
    /// provider-using workflow runs with no real agent CLI or authentication.
    pub mock: bool,
}

/// Runs the workflow named by `args.file` (a path or a recipe name). Exit: `0` succeeded,
/// `1` failed / invalid, `2` parse/IO.
pub(crate) fn run(args: RunArgs) -> anyhow::Result<ExitCode> {
    let file = crate::catalog::resolve_arg(&args.file, args.recipes_dir.as_deref())?;
    let src =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let workflow = match Workflow::from_yaml_str(&src) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("✗ {}: parse error\n  {e}", file.display());
            return Ok(ExitCode::from(2));
        }
    };

    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    runtime.block_on(execute(&workflow, args))
}

async fn execute(workflow: &Workflow, args: RunArgs) -> anyhow::Result<ExitCode> {
    let repo = args.repo.clone().unwrap_or_else(|| PathBuf::from("."));

    // An `approval:` gate pauses the run and is resumed *from the store* on a decision; with
    // `--no-store` the gate could never be approved, and the "run `odin approve …`" hint printed
    // below would be unusable. Refuse up front rather than launch an unresumable run.
    if args.no_store
        && workflow
            .steps
            .iter()
            .any(|s| matches!(s.kind, StepKind::Approval(_)))
    {
        anyhow::bail!(
            "--no-store cannot be used with a workflow that has an `approval:` gate: a paused gate \
             is persisted to (and resumed from) the store, so without one the run could never be \
             approved. Drop --no-store, or remove the gate."
        );
    }

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

    if args.mock {
        register_mock_providers(&mut builder, workflow);
        eprintln!(
            "note: --mock — provider steps echo their rendered prompt; no real agent CLI is invoked"
        );
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

/// Registers a prompt-echoing [`odin_core::mock::EchoProvider`] for every provider the workflow
/// references (provider steps, their judges, and loop-body steps), overriding the real CLI
/// adapters — so `--mock` runs the whole workflow with no agent CLI or authentication.
fn register_mock_providers(builder: &mut EngineBuilder, workflow: &Workflow) {
    let mut names = std::collections::BTreeSet::new();
    for step in &workflow.steps {
        add_provider_names(step, &mut names);
        if let StepKind::Loop(l) = &step.kind {
            for inner in &l.steps {
                add_provider_names(inner, &mut names);
            }
        }
    }
    for name in &names {
        builder
            .registry_mut()
            .register_provider(Arc::new(odin_core::mock::EchoProvider::new(name.as_str())));
    }
}

/// Collects the provider names a single step references (a provider step's `provider:` and any
/// `judge:` provider) into `names`.
fn add_provider_names(step: &Step, names: &mut std::collections::BTreeSet<String>) {
    if let StepKind::Provider(p) = &step.kind {
        names.insert(p.provider.as_str().to_owned());
    }
    if let Some(judge) = &step.judge {
        names.insert(judge.provider.as_str().to_owned());
    }
}
