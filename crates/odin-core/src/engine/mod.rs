//! The [`Engine`] façade and its builder, plus the built-in linear executor.
//!
//! Embedders construct an engine with [`EngineBuilder`], register any custom plugins,
//! and drive workflows via [`Engine::run`] / [`Engine::resume_all`].

mod local;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::api::{Decision, RerunOutcome, RunInput, RunSummary};
use crate::error::Result;
use crate::ids::RunId;
use crate::ir::Workflow;
use crate::registry::Registry;
use crate::traits::{PrunePolicy, PruneReport, Store};

/// The thing embedders drive to run workflows.
#[async_trait]
pub trait Engine: Send + Sync {
    /// Runs a workflow to completion, returning the structured summary.
    ///
    /// Validates the workflow first (returning [`crate::error::Error::Validation`] on
    /// errors), then resolves the run's params ([`crate::error::Error::Input`] if a required
    /// one is missing), and checkpoints to the [`Store`] if the workflow is durable.
    ///
    /// # Errors
    /// Returns an [`crate::error::Error`] if validation, param resolution, a plugin, or
    /// persistence fails.
    async fn run(&self, workflow: &Workflow, input: RunInput) -> Result<RunSummary>;

    /// Requests cancellation of an in-flight run: fires its cancel token so the running step's
    /// subprocess is killed and the run ends as
    /// [`RunStatus::Cancelled`](crate::api::RunStatus::Cancelled). Cooperative — the run stops
    /// launching new steps at the next scheduling boundary. Returns `true` if a matching in-flight
    /// run was found and signalled, `false` for an unknown id or a run that is already
    /// terminal/paused.
    fn cancel_run(&self, run_id: RunId) -> bool;

    /// Cancels **every** in-flight run (see [`cancel_run`](Engine::cancel_run)) and returns how
    /// many were signalled. The daemon uses this to stop in-flight work promptly on shutdown;
    /// `durable` runs resume on the next start.
    fn cancel_all_active(&self) -> usize;

    /// Resumes any incomplete runs found in the [`Store`] (crash recovery).
    ///
    /// # Errors
    /// Returns an [`crate::error::Error`] if recovery fails.
    async fn resume_all(&self, workflows: &[Workflow]) -> Result<Vec<RunSummary>>;

    /// Fetches the summary of a known run id from the [`Store`].
    ///
    /// # Errors
    /// Returns an [`crate::error::Error`] if the store read fails.
    async fn summary(&self, run_id: RunId) -> Result<Option<RunSummary>>;

    /// Records a human `decision` on the run's pending [`approval`](crate::ir::ApprovalStep)
    /// gate, then resumes the run — returning its resulting summary (terminal, or paused again
    /// at a later gate). `Ok(None)` if the run id is unknown. `workflows` must include the run's
    /// own workflow definition (needed to resume).
    ///
    /// # Errors
    /// Returns [`crate::error::Error::Input`] if the run is not awaiting approval, or another
    /// [`crate::error::Error`] if the store, resume, or a plugin fails.
    async fn submit_approval(
        &self,
        run_id: RunId,
        decision: Decision,
        approver: String,
        note: Option<String>,
        workflows: &[Workflow],
    ) -> Result<Option<RunSummary>>;

    /// Rejects the run's pending gate (failing it, carrying `note` as the feedback) and then
    /// starts a FRESH run of the same workflow with `note` injected as the `feedback` param,
    /// plus the original run's params/trigger — so the workflow can address the feedback and
    /// try again. Returns both summaries. `Ok(None)` if the run id is unknown. `workflows` must
    /// include the run's own workflow definition.
    ///
    /// # Errors
    /// Returns [`crate::error::Error::Input`] if the run is not awaiting approval, or another
    /// [`crate::error::Error`] if the store, the reject, or starting the new run fails.
    async fn reject_and_rerun(
        &self,
        run_id: RunId,
        approver: String,
        note: String,
        workflows: &[Workflow],
    ) -> Result<Option<RerunOutcome>>;

    /// Applies a retention `policy`: prunes matching **terminal** runs from the [`Store`] (never
    /// non-terminal ones) and reclaims each pruned run's leftover git snapshot refs from the
    /// repo. With `dry_run`, reports what would be pruned and changes nothing. Returns the
    /// [`PruneReport`] (empty if no store is configured).
    ///
    /// # Errors
    /// Returns a [`crate::error::Error`] if the store read/write fails.
    async fn prune(&self, policy: &PrunePolicy, dry_run: bool) -> Result<PruneReport>;
}

/// Wires a [`Registry`] of plugins, a repository root, and a [`Store`] into an engine.
#[derive(Default)]
pub struct EngineBuilder {
    registry: Registry,
    store: Option<Arc<dyn Store>>,
    repo_root: Option<PathBuf>,
}

impl EngineBuilder {
    /// A new builder seeded with the built-in providers/workspaces/actions.
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: Registry::with_builtins(),
            store: None,
            repo_root: None,
        }
    }

    /// Sets the git repository the engine provisions workspaces from. Defaults to `.`.
    #[must_use]
    pub fn repo(mut self, repo_root: impl Into<PathBuf>) -> Self {
        self.repo_root = Some(repo_root.into());
        self
    }

    /// Provides the durable store. Without one, runs are not checkpointed.
    #[must_use]
    pub fn store(mut self, store: Arc<dyn Store>) -> Self {
        self.store = Some(store);
        self
    }

    /// Accesses the registry to register custom plugins.
    pub fn registry_mut(&mut self) -> &mut Registry {
        &mut self.registry
    }

    /// Finalizes into a runnable engine.
    ///
    /// # Errors
    /// Currently infallible, but returns `Result` so future validation (e.g. required
    /// plugins) is non-breaking.
    pub fn build(self) -> Result<Arc<dyn Engine>> {
        let repo_root = self.repo_root.unwrap_or_else(|| PathBuf::from("."));
        // Absolutize so providers/workspaces never receive a *relative* working directory.
        // Tools like `codex --cd`/`-o` resolve their path arguments relative to the child's
        // own cwd — which the engine has already set to the workdir — so a relative workdir
        // doubles into a nonexistent path and the CLI fails with "No such file or directory".
        // `absolute` (unlike `canonicalize`) doesn't resolve symlinks, keeping paths stable.
        let repo_root = std::path::absolute(&repo_root).unwrap_or(repo_root);
        Ok(Arc::new(local::LocalEngine::new(
            self.registry,
            self.store,
            repo_root,
        )))
    }
}
