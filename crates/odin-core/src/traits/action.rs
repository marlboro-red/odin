//! The [`Action`] trait: a built-in, named side-effect step.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::Value;

use crate::api::SideEffect;
use crate::error::ActionError;
use crate::ids::StepId;
use crate::traits::CancelToken;

/// A first-class, reusable side-effect available by name in `action:`
/// (`github.open_pr`, `git.commit`, `shell.exec`). Distinct from a non-deterministic
/// [`Provider`](crate::traits::Provider) and from an arbitrary `run:` hook.
#[async_trait]
pub trait Action: Send + Sync {
    /// The name authors reference in `action:`.
    fn name(&self) -> &str;

    /// Executes the action against the prepared context.
    ///
    /// # Errors
    /// Returns an [`ActionError`] if the action fails.
    async fn run(&self, ctx: ActionCtx) -> Result<ActionOutcome, ActionError>;
}

/// Everything an action needs.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ActionCtx {
    /// The step being run.
    pub step_id: StepId,
    /// Working directory.
    pub workdir: PathBuf,
    /// The step's `with:` args, already templated.
    pub args: IndexMap<String, Value>,
    /// Fires when the run is cancelled (Ctrl-C, daemon shutdown). An action that shells out
    /// MUST pass this to its subprocesses so a hung command (e.g. an interactive auth prompt)
    /// can be killed instead of wedging the whole run.
    pub cancel: CancelToken,
    /// The step's wall-clock timeout, if any — apply it to subprocesses for the same reason.
    pub timeout: Option<Duration>,
}

/// What an action produced.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct ActionOutcome {
    /// 0 = success. Mirrors the provider/run convention so gate logic is uniform.
    pub exit_code: i32,
    /// Outputs exposed to later steps as `steps.<id>.outputs.*`.
    pub outputs: IndexMap<String, Value>,
    /// Captured stderr, folded into the step's failure reason / `retry.feedback` when the action
    /// fails (a non-zero `exit_code`) — so a failed `shell.exec` keeps its actual error.
    pub stderr: String,
    /// Externally-visible effects, surfaced in [`crate::api::RunSummary`].
    pub side_effects: Vec<SideEffect>,
}

impl ActionOutcome {
    /// A successful (exit 0) outcome with no outputs or side effects — the starting point for
    /// a custom [`Action`]. Because the struct is `#[non_exhaustive]`, out-of-crate
    /// implementors build it with this plus the `with_*` builders rather than a struct literal.
    #[must_use]
    pub fn success() -> Self {
        Self::default()
    }

    /// Records an outward [`SideEffect`] (builder style), surfaced in the run summary.
    #[must_use]
    pub fn with_side_effect(mut self, effect: SideEffect) -> Self {
        self.side_effects.push(effect);
        self
    }

    /// Adds a named output exposed to later steps as `steps.<id>.outputs.<key>` (builder style).
    #[must_use]
    pub fn with_output(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.outputs.insert(key.into(), value.into());
        self
    }
}
