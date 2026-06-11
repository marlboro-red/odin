//! The static shape of the template context derived from a workflow.

use indexmap::IndexSet;

use crate::ir::Workflow;

/// The set of names a template may legally reference, derived from a [`Workflow`].
///
/// Per-step gating (a `steps.<id>` ref must be an upstream dependency; an
/// `artifacts.<NAME>` ref must be in the step's `requires`) is applied by the
/// [`super::refs`] checker using the dependency graph; this type holds the
/// workflow-wide sets.
#[derive(Clone, Debug)]
pub struct ContextShape {
    /// Declared parameter names.
    pub params: IndexSet<String>,
    /// Declared step ids.
    pub steps: IndexSet<String>,
    /// Params whose value is **mapped from a webhook trigger** (`github_webhook` / `webhook`), so
    /// they carry untrusted, attacker-controlled payload content — the same trust level as
    /// `trigger.*`. Drives the shell-injection lint (ODIN046) for the `params.*` path.
    pub webhook_params: IndexSet<String>,
}

impl ContextShape {
    /// Derives the context shape from a workflow.
    #[must_use]
    pub fn of(wf: &Workflow) -> Self {
        use crate::ir::TriggerDecl;
        let webhook_params = wf
            .triggers
            .iter()
            .filter_map(|t| match t {
                TriggerDecl::GithubWebhook(g) => Some(&g.params),
                TriggerDecl::Webhook(w) => Some(&w.params),
                _ => None,
            })
            .flat_map(|params| params.keys().map(|k| k.as_str().to_owned()))
            .collect();
        Self {
            params: wf.params.keys().map(|k| k.as_str().to_owned()).collect(),
            steps: wf.steps.iter().map(|s| s.id.as_str().to_owned()).collect(),
            webhook_params,
        }
    }
}
