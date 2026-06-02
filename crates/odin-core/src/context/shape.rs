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
}

impl ContextShape {
    /// Derives the context shape from a workflow.
    #[must_use]
    pub fn of(wf: &Workflow) -> Self {
        Self {
            params: wf.params.keys().map(|k| k.as_str().to_owned()).collect(),
            steps: wf.steps.iter().map(|s| s.id.as_str().to_owned()).collect(),
        }
    }
}
