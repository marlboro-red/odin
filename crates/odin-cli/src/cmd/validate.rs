//! The `odin validate` subcommand: parse + validate a workflow and report diagnostics.

use std::path::Path;
use std::process::ExitCode;

use anyhow::Context as _;
use odin_core::{KnownNames, ValidationReport, Workflow, validate_source};

use crate::catalog;

/// Validates the workflow at `arg` — either a file path or a recipe name (see
/// [`catalog::resolve_arg`]). Returns the process exit code: `0` valid (possibly with warnings),
/// `1` validation errors, `2` parse/IO failure.
pub(crate) fn run(arg: &Path, recipes_dir: Option<&Path>, json: bool) -> anyhow::Result<ExitCode> {
    let file = catalog::resolve_arg(arg, recipes_dir)?;
    let file = file.as_path();
    let src =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;

    let wf = match Workflow::from_yaml_str(&src) {
        Ok(wf) => wf,
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json_parse_envelope(&e.to_string()))?
                );
            } else {
                eprintln!("✗ {}: parse error", file.display());
                eprintln!("  {e}");
            }
            return Ok(ExitCode::from(2));
        }
    };

    let report = validate_source(&src, &wf, &KnownNames::builtin());

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json_validation_envelope(&report))?
        );
    } else {
        print_human(file, &report);
    }

    Ok(if report.has_errors() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// The unified `--json` envelope for a validation result: `{ ok, phase: "validate", diagnostics,
/// error: null }`. Shared with `odin run --json` so both emit the same shape on a validation
/// failure. `ok` is true iff there are no error-severity diagnostics (warnings keep `ok: true`).
pub(crate) fn json_validation_envelope(report: &ValidationReport) -> serde_json::Value {
    serde_json::json!({
        "ok": !report.has_errors(),
        "phase": "validate",
        "diagnostics": &report.diagnostics,
        "error": serde_json::Value::Null,
    })
}

/// The unified `--json` envelope for a parse failure: `{ ok: false, phase: "parse", diagnostics:
/// [], error }`. Same top-level keys as [`json_validation_envelope`] so one consumer handles both.
pub(crate) fn json_parse_envelope(error: &str) -> serde_json::Value {
    serde_json::json!({
        "ok": false,
        "phase": "parse",
        "diagnostics": [],
        "error": error,
    })
}

fn print_human(file: &Path, report: &ValidationReport) {
    if report.is_empty() {
        println!("✓ {} is valid", file.display());
        return;
    }
    for d in &report.diagnostics {
        println!("{d}\n");
    }
    let (errors, warnings) = (report.error_count(), report.warning_count());
    if report.has_errors() {
        println!(
            "✗ {}: {errors} error(s), {warnings} warning(s)",
            file.display()
        );
    } else {
        println!("✓ {} is valid ({warnings} warning(s))", file.display());
    }
}
