//! The `odin validate` subcommand: parse + validate a workflow and report diagnostics.

use std::path::Path;
use std::process::ExitCode;

use anyhow::Context as _;
use odin_core::{KnownNames, ValidationReport, Workflow, validate_source};

/// Validates the workflow at `file`. Returns the process exit code:
/// `0` valid (possibly with warnings), `1` validation errors, `2` parse/IO failure.
pub(crate) fn run(file: &Path, json: bool) -> anyhow::Result<ExitCode> {
    let src =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;

    let wf = match Workflow::from_yaml_str(&src) {
        Ok(wf) => wf,
        Err(e) => {
            if json {
                let obj = serde_json::json!({
                    "ok": false,
                    "phase": "parse",
                    "error": e.to_string(),
                });
                println!("{}", serde_json::to_string_pretty(&obj)?);
            } else {
                eprintln!("✗ {}: parse error", file.display());
                eprintln!("  {e}");
            }
            return Ok(ExitCode::from(2));
        }
    };

    let report = validate_source(&src, &wf, &KnownNames::builtin());

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human(file, &report);
    }

    Ok(if report.has_errors() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
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
