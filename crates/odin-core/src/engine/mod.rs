//! The [`Engine`] façade and its builder, plus the built-in linear executor.
//!
//! Embedders construct an engine with [`EngineBuilder`], register any custom plugins,
//! and drive workflows via [`Engine::run`] / [`Engine::resume_all`].

mod local;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::api::{RunInput, RunSummary};
use crate::error::Result;
use crate::ids::RunId;
use crate::ir::Workflow;
use crate::registry::Registry;
use crate::traits::Store;

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
