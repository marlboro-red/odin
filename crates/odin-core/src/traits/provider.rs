//! The [`Provider`] trait: invoke an autonomous coding-agent CLI for a step.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::Value;

use crate::error::ProviderError;
use crate::ids::{ArtifactName, ProviderRef, StepId};
use crate::usage::Usage;

/// An autonomous coding-agent CLI Odin can invoke for a `provider:` step.
///
/// Implement this in one file: you receive an [`InvocationCtx`] (rendered prompt,
/// workdir, resolved inputs) and return an [`InvocationOutcome`] (exit code, captured
/// output, usage, produced artifacts). The engine owns durability and `DIFF` capture; a
/// provider must not touch the [`crate::traits::Store`] or git directly.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Registry key this provider answers to (e.g. `"claude"`). Must match `provider:` pins.
    fn id(&self) -> ProviderRef;

    /// Runs the agent against the workspace. The one required method.
    ///
    /// # Errors
    /// Returns a [`ProviderError`] if the CLI is missing, times out, or fails.
    async fn invoke(&self, ctx: InvocationCtx) -> Result<InvocationOutcome, ProviderError>;

    /// Best-effort CLI version string, recorded in run state for reproducibility.
    async fn version(&self) -> Option<String> {
        None
    }

    /// Cheap readiness probe (CLI installed & authed?). Defaults to assuming OK.
    ///
    /// # Errors
    /// Returns a [`ProviderError`] if the provider is not ready to run.
    async fn health_check(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// Everything a provider needs for one invocation. Owned so it can cross crate/process
/// boundaries; the prompt is already fully rendered by the engine.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct InvocationCtx {
    /// The step being run.
    pub step_id: StepId,
    /// Working directory (the acquired workspace path).
    pub workdir: PathBuf,
    /// Fully-rendered prompt. `None` only for prompt-from-artifact steps.
    pub prompt: Option<String>,
    /// Required artifacts, resolved to on-disk paths.
    pub inputs: IndexMap<ArtifactName, PathBuf>,
    /// Per-step timeout; the provider should self-limit, the engine hard-kills.
    pub timeout: Option<Duration>,
    /// Fires on run cancel/timeout. Honor it and return promptly.
    pub cancel: CancelToken,
}

/// What a provider produced.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct InvocationOutcome {
    /// Process exit code (0 = success by convention).
    pub exit_code: i32,
    /// Captured stdout (the agent's textual result; may be read by a judge).
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// Structured outputs exposed to later steps as `steps.<id>.outputs.*`.
    pub outputs: IndexMap<String, Value>,
    /// Token/cost usage, if the CLI reports it.
    pub usage: Option<Usage>,
    /// Artifacts explicitly written (name → path). The engine still auto-captures `DIFF`.
    pub produced: IndexMap<ArtifactName, PathBuf>,
}

impl InvocationOutcome {
    /// Convenience constructor for trivial/mock providers: exit 0 with the given stdout.
    #[must_use]
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            exit_code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
            outputs: IndexMap::new(),
            usage: Some(Usage::default()),
            produced: IndexMap::new(),
        }
    }
}

/// A clonable cancellation handle wrapping a `tokio_util` cancellation token, so the
/// public signatures do not leak that dependency. Clones share one cancellation state.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(pub(crate) tokio_util::sync::CancellationToken);

impl CancelToken {
    /// A fresh, un-cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation for this token and all of its clones.
    pub fn cancel(&self) {
        self.0.cancel();
    }

    /// True once cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    /// Resolves when cancellation is requested. Cancel-safe.
    pub async fn cancelled(&self) {
        self.0.cancelled().await;
    }
}
