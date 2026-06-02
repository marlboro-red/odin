//! Semantic validation of a parsed [`crate::ir::Workflow`].
//!
//! Parsing (in [`crate::ir`]) is fail-fast and catches *structural* errors (bad YAML,
//! unknown leaf fields, a step with two kinds). Validation is the second phase: it runs
//! every rule and **collects** all [`Diagnostic`]s so an author sees every problem at
//! once. Each rule maps to a stable `ODIN###` [`DiagCode`].

pub mod diagnostic;
pub mod graph;
pub(crate) mod rules;

pub use diagnostic::{DiagCode, Diagnostic, Severity, ValidationReport};

use crate::ir::Workflow;

/// The set of registered plugin names validation checks references against.
///
/// A parse-only build (feature `ir`, no `runtime`) uses [`KnownNames::builtin`]; with
/// the `runtime` feature a [`KnownNames`] can be derived from a live
/// [`crate::registry::Registry`] so third-party plugins are recognized.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct KnownNames<'a> {
    /// Registered provider keys (e.g. `"claude"`).
    pub providers: Vec<&'a str>,
    /// Registered action names (e.g. `"github.open_pr"`).
    pub actions: Vec<&'a str>,
}

impl<'a> KnownNames<'a> {
    /// Constructs a name set from provider and action names.
    ///
    /// Prefer this over a struct literal: `KnownNames` is `#[non_exhaustive]`, so future
    /// name categories (triggers, workspaces) can be added without breaking callers.
    #[must_use]
    pub fn new(providers: Vec<&'a str>, actions: Vec<&'a str>) -> Self {
        Self { providers, actions }
    }
}

impl KnownNames<'static> {
    /// The built-in provider/action names Odin ships with.
    ///
    /// These let a parse-only linter validate references before the runtime plugin
    /// implementations exist. The `runtime` engine validates against its live registry
    /// instead, so third-party plugins are also recognized.
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            providers: vec!["claude", "codex", "copilot"],
            actions: vec!["github.open_pr", "git.commit", "git.push", "shell.exec"],
        }
    }
}

impl Default for KnownNames<'static> {
    fn default() -> Self {
        Self::builtin()
    }
}

/// Validates a workflow, collecting every [`Diagnostic`] in a single pass.
///
/// This is the semantic phase only; it assumes the workflow already parsed. Use
/// [`validate_source`] when you also have the raw YAML and want the root-unknown-field
/// warning (`ODIN025`).
#[must_use]
pub fn validate(wf: &Workflow, known: &KnownNames<'_>) -> ValidationReport {
    let mut d = Vec::new();
    let ancestors = graph::ancestor_sets(wf);

    rules::step_list_nonempty(wf, &mut d);
    rules::step_ids(wf, &mut d);
    rules::provider_refs(wf, known, &mut d);
    rules::prompts(wf, &mut d);
    rules::actions(wf, known, &mut d);
    rules::judge(wf, &mut d);
    rules::depends_on(wf, &mut d);
    rules::cycles(wf, &mut d);
    rules::artifacts(wf, &ancestors, &mut d);
    rules::workspace(wf, &mut d);
    rules::triggers(wf, &mut d);
    rules::params(wf, &mut d);
    rules::retry_fallback(wf, &mut d);
    rules::schema(wf, &mut d);

    #[cfg(feature = "templating")]
    crate::context::refs::check(wf, &ancestors, &mut d);

    ValidationReport { diagnostics: d }
}

/// Like [`validate`], but also inspects the raw YAML `src` for unknown root-level keys
/// (`ODIN025`), which the typed parse silently tolerates for forward-compatibility.
#[must_use]
pub fn validate_source(src: &str, wf: &Workflow, known: &KnownNames<'_>) -> ValidationReport {
    let mut report = validate(wf, known);
    rules::root_unknown_fields(src, &mut report.diagnostics);
    report
}

#[cfg(test)]
mod tests {
    use super::{DiagCode, KnownNames, validate};
    use crate::ir::Workflow;

    fn report(yaml: &str) -> super::ValidationReport {
        let wf = Workflow::from_yaml_str(yaml).unwrap();
        validate(&wf, &KnownNames::builtin())
    }

    #[test]
    fn clean_workflow_has_no_errors() {
        let r = report(
            "name: ok\nsteps:\n  - {id: a, provider: claude, prompt: hi}\n  - {id: b, run: ./x.sh, depends_on: [a]}\n",
        );
        assert!(!r.has_errors(), "{r}");
    }

    #[test]
    fn unknown_provider_is_flagged_with_suggestion() {
        let r = report("name: x\nsteps:\n  - {id: a, provider: claud, prompt: hi}\n");
        assert!(r.contains(DiagCode::UnknownProvider));
        let diag = r.by_code(DiagCode::UnknownProvider).next().unwrap();
        assert!(diag.help.as_ref().unwrap().contains("claude"));
    }

    #[test]
    fn empty_workflow_reports_no_steps() {
        let r = report("name: x\nsteps: []\n");
        assert!(r.contains(DiagCode::NoSteps));
    }
}
