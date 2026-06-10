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
    /// Live-output sink for this invocation. When `Some`, the provider should pass it into its
    /// [`ProcessOptions::stream`](crate::provider::ProcessOptions::stream) so the agent CLI's
    /// output is teed to the terminal as it runs. `None` (the default) = capture only.
    pub stream: Option<crate::provider::process::StreamSink>,
}

impl InvocationCtx {
    /// A context with the given step and workdir and otherwise-empty defaults (no prompt, no
    /// inputs, no timeout, a fresh cancel token, capture-only). The struct is `#[non_exhaustive]`,
    /// so this constructor — not a literal — is how external code (e.g. a unit test of a custom
    /// [`Provider`]) builds one; set the remaining `pub` fields you need on the returned value.
    ///
    /// ```
    /// use odin_core::{InvocationCtx, StepId};
    /// let mut ctx = InvocationCtx::new(StepId::new("plan"), std::env::temp_dir());
    /// ctx.prompt = Some("hello".to_owned());
    /// assert_eq!(ctx.step_id.as_str(), "plan");
    /// ```
    #[must_use]
    pub fn new(step_id: StepId, workdir: PathBuf) -> Self {
        Self {
            step_id,
            workdir,
            prompt: None,
            inputs: IndexMap::new(),
            timeout: None,
            cancel: CancelToken::new(),
            stream: None,
        }
    }
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
    /// A successful (exit 0) outcome carrying `stdout` (and default usage). The struct is
    /// `#[non_exhaustive]`, so out-of-crate providers build outcomes with this / [`failure`]
    /// plus the `with_*` builders rather than a struct literal.
    ///
    /// [`failure`]: Self::failure
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

    /// A failed outcome with the given non-zero exit code (no stdout/usage). Pair with
    /// [`with_stderr`](Self::with_stderr) to surface the CLI's error.
    #[must_use]
    pub fn failure(exit_code: i32) -> Self {
        Self {
            exit_code,
            stdout: String::new(),
            stderr: String::new(),
            outputs: IndexMap::new(),
            usage: None,
            produced: IndexMap::new(),
        }
    }

    /// Sets the process exit code (builder style); the engine fails the step on non-zero.
    #[must_use]
    pub fn with_exit_code(mut self, exit_code: i32) -> Self {
        self.exit_code = exit_code;
        self
    }

    /// Sets captured stderr (builder style).
    #[must_use]
    pub fn with_stderr(mut self, stderr: impl Into<String>) -> Self {
        self.stderr = stderr.into();
        self
    }

    /// Adds a structured output exposed as `steps.<id>.outputs.<key>` (builder style).
    #[must_use]
    pub fn with_output(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.outputs.insert(key.into(), value.into());
        self
    }

    /// Sets token/cost usage (builder style).
    #[must_use]
    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = Some(usage);
        self
    }

    /// Records an artifact this invocation wrote (name → path; builder style).
    #[must_use]
    pub fn with_produced(mut self, name: ArtifactName, path: PathBuf) -> Self {
        self.produced.insert(name, path);
        self
    }
}

/// A clonable cancellation handle wrapping a `tokio_util` cancellation token, so the
/// public signatures do not leak that dependency. Clones share one cancellation state.
///
/// Cancellation carries a *reason* so the engine can tell a user-initiated cancel (the run
/// ends terminally `Cancelled`) from a graceful-shutdown interrupt (a `durable` run is left
/// resumable, not killed). The reason is shared across clones alongside the token.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(
    pub(crate) tokio_util::sync::CancellationToken,
    std::sync::Arc<std::sync::atomic::AtomicU8>,
);

/// Reason values stored in the token's shared `AtomicU8`.
mod reason {
    /// Not cancelled (or cancelled with no recorded reason).
    pub(super) const NONE: u8 = 0;
    /// User-initiated cancel — the run ends terminally `Cancelled`.
    pub(super) const CANCEL: u8 = 1;
    /// Graceful-shutdown interrupt — a durable run is left resumable.
    pub(super) const SHUTDOWN: u8 = 2;
}

impl CancelToken {
    /// A fresh, un-cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation for this token and all of its clones (user-initiated): the run
    /// ends terminally [`Cancelled`](crate::api::RunStatus::Cancelled). A user cancel takes
    /// precedence over a prior shutdown reason (terminating is always safe).
    pub fn cancel(&self) {
        self.1
            .store(reason::CANCEL, std::sync::atomic::Ordering::SeqCst);
        self.0.cancel();
    }

    /// Requests cancellation as a **graceful shutdown**: the in-flight step's subprocess is
    /// killed promptly (like [`cancel`](Self::cancel)), but a durable run is checkpointed
    /// non-terminal so it resumes on the next start rather than dying as `Cancelled`. Does not
    /// override a reason already set by [`cancel`](Self::cancel).
    pub fn shutdown(&self) {
        let _ = self.1.compare_exchange(
            reason::NONE,
            reason::SHUTDOWN,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        );
        self.0.cancel();
    }

    /// True once cancellation has been requested (for any reason).
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    /// True when cancellation was a graceful [`shutdown`](Self::shutdown) (not a user
    /// [`cancel`](Self::cancel)) — the engine uses this to leave a durable run resumable.
    ///
    /// Only the engine (gated on `runtime` + `templating`) reads this; allow it to be unused in a
    /// runtime-without-templating build so the per-feature clippy pass stays clean.
    #[cfg_attr(not(feature = "templating"), allow(dead_code))]
    #[must_use]
    pub(crate) fn is_shutdown(&self) -> bool {
        self.0.is_cancelled()
            && self.1.load(std::sync::atomic::Ordering::SeqCst) == reason::SHUTDOWN
    }

    /// Resolves when cancellation is requested. Cancel-safe.
    pub async fn cancelled(&self) {
        self.0.cancelled().await;
    }
}
